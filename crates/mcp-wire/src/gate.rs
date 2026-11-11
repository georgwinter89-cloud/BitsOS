//! MC-8a: the gate, `/dev/bitsos/mcp` (root only, 0600, misc 10:238), the kernel's narrow interface for the
//! user-space MCP service `bitsmcp`. The service speaks HTTP and JSON-RPC itself (`http`, `rpc`); the kernel
//! answers only these commands, one per write(2), its answer read back from the start (the next write replaces
//! it; a failure is the write's errno):
//!
//! | command | answer |
//! |---|---|
//! | `hello` | `{"mode":"both","port":8766,"conns":8,"version":"0.1.0"}`: where the service listens (`mcp=both`: `mcp_port`+1, `mcp=service`: `mcp_port`) |
//! | `list [<client>]` | `{"tools":[{"name","description","schema"(text),"owner","safety","verified","pid","offer"}]}`: the tools the gate offers (`offer` below; to a client with a live full-access grant every kernel tool, `offer_for`, MC-10) |
//! | `auth <peer> <token>` | `ok`, or EACCES (the kernel keeps the token; the service never reads it) |
//! | `call {"tool","arguments","timeout_ms","peer","client"}` | the tool's result (`encode_result`); waits until the result or the call's deadline (+0.25 s, then "Still running") |
//! | `install {"tool","arguments","timeout_ms","peer","client"}` | for the installer (MC-9a, `crate::installer`: install, Reload, pin, reboot, guarded settings, the NVMe gate): waits for its yes or no, then the tool's result or the refusal; EPERM while no installer runs |
//! | `claim`, `next`, `decide`, `grants {...}` | the installer's session only (`crate::installer`; `grants`: the full-access table, `crate::access`, MC-10) |
//! | `access <client>` | MC-10: that client's grant (`access::Status`); EACCES before `auth` |
//! | `answered` | `ok`: an answer of an authenticated request went out (`mcp.answered`, RB-6's boot confirmation); EPERM without an `auth` since the last one |
//!
//! Every `call` and `install` is a kernel event (`mcp svc.call`, `mcp svc.install_refused`: tool, peer, result),
//! so bitsjournal keeps each MCP action. Nothing else crosses: no raw memory, no MMIO, no program start, no
//! file access; `offer` is the one table both sides read.
//!
//! MC-10: `client` is the SHA-256 of the TLS client certificate bitsmcp saw (64 hex digits; absent for a plain
//! connection). Only a client with a live full-access grant (`crate::access`, set at the Mini-PC's keyboard
//! through the installer) gets every kernel tool; every other client keeps the narrow set.

use crate::{Json, Safety, ToolError, ToolOutput, ToolResult, b64, json, rpc};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

pub const NODE: &str = "/dev/bitsos/mcp";
/// misc major 10; a minor no Linux driver uses (10:239 is /dev/bitssnap, 10:240 /dev/bitsos/events).
pub const DEV: (u32, u32) = (10, 238);
/// Longest command (a call's arguments come from one request body, at most `http::BODY_MAX`).
pub const LINE_MAX: usize = crate::http::BODY_MAX + 4096;
/// How `bitsmcp` and init learn the mode without the node: `/proc/bitsos/mcp` (0444), `mode=<m> port=<p>`.
pub const PROC_FILE: &str = "/proc/bitsos/mcp";

/// CONFIG.TXT `mcp=`: who serves MCP (MC-8a transition; default `kernel`, today's behaviour).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The kernel's server on `mcp_port`; no gate.
    Kernel,
    /// The kernel's server on `mcp_port` and the service on `mcp_port`+1.
    Both,
    /// Only the service, on `mcp_port`. MC-10: this is the developer setup the Mini-PC runs (Georg 27.10.):
    /// TLS only (no plain-HTTP fallback: a leaked token alone opens nothing), every pairing asked at the real
    /// keyboard, the narrow set until Georg grants full access at the keyboard. `kernel` stays the rescue
    /// setting he can write on the stick; `both` the transition.
    Service,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim() {
            "kernel" => Some(Mode::Kernel),
            "both" => Some(Mode::Both),
            "service" => Some(Mode::Service),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Mode::Kernel => "kernel",
            Mode::Both => "both",
            Mode::Service => "service",
        }
    }
    /// The kernel's own server runs.
    pub fn kernel_serves(self) -> bool {
        self != Mode::Service
    }
    /// The gate is open and init starts the service.
    pub fn service_serves(self) -> bool {
        self != Mode::Kernel
    }
    /// The service's port for `mcp_port` (None: `both` on port 65535).
    pub fn service_port(self, mcp_port: u16) -> Option<u16> {
        match self {
            Mode::Kernel => None,
            Mode::Both => mcp_port.checked_add(1),
            Mode::Service => Some(mcp_port),
        }
    }
}

/// What the service does with a kernel tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offer {
    /// Answered by the kernel through `call`.
    Gate,
    /// Listed with its schema, called through `install` (the installer asks first, MC-9a).
    Installer,
    /// Not offered; the reason is the call's answer.
    Dropped(&'static str),
}

/// The narrow set (MC-8a brief): hw_info and metrics, events and the kernel ring, Safe-class ktests with their
/// `output`/`stop`, and the start's identity (`version`, `reload_info`, `system_list`: read-only, what a desk
/// round needs to follow an install or Reload). `gpu` (GX-9b): the clocks and limits, class Read, one read-only
/// table transfer from the SMU.
const GATE: &[&str] = &["events", "gpu", "hw_info", "ktest", "log", "metrics", "output", "reload_info", "stop", "system_list", "version"];
// MC-9a: `nvme_gate` (opening the NVMe write gate at run time) has no tool yet; named here so that it goes to the
// installer, never through the gate, once one is registered.
const INSTALLER: &[&str] = &["install_kernel", "install_system", "nvme_gate", "reboot", "reload", "system_pin"];
const RAW: &[&str] = &["dcn_cursor", "dcn_flip", "dcn_snapshot", "dcn_writes", "gx_probe", "mmio_read", "mmio_write", "msr_read", "pci_read", "pci_rescan", "pci_write", "read_phys"];
const PROCESSES: &[&str] = &["kill", "ps", "run"];
const FILES: &[&str] = &["delete", "fetch", "list_dir", "manifest", "move_tree", "read_file", "stat", "write_file"];
const CONSOLE: &[&str] = &["screen", "send_keys", "send_mouse"];
const DEVICES: &[&str] = &["crash_test", "data_move", "display_mode", "hang", "preload", "sched_focus", "snd", "wlan"];
const OTHER: &[&str] = &["connections", "cpus", "crash_info", "input", "latency", "locks", "net_info", "nvme", "sched", "watchdog"];

const WHY_RAW: &str = "raw memory, registers, MMIO or PCI config space never go through the user-space MCP service";
const WHY_PROCESSES: &str = "starts or signals processes as root; a narrow program start under policy comes with MC-8b";
const WHY_FILES: &str = "acts as the kernel on every path, the boot stick included; writes go to the installer (MC-9), reads wait for the service's own mount view (SX-2)";
const WHY_CONSOLE: &str = "reaches the console's frame and input queue directly; injected input is MC-9's confirmation question";
const WHY_DEVICES: &str = "changes device or kernel state outside the installer's list";
const WHY_OTHER: &str = "kernel internals beyond the gate's narrow set (metrics, hw_info, events, the journal, Safe-class ktests); it may come back as a narrow call when a desk round needs it";

/// The service's table for every kernel tool (unknown ones: dropped, fail closed). Under `mcp=both` the kernel's
/// own server on `mcp_port` keeps offering all of them.
pub fn offer(tool: &str) -> Offer {
    named(tool).unwrap_or(Offer::Dropped(WHY_OTHER))
}

/// MC-10: the table for a client; with a live full-access grant every kernel tool is a call through the gate (as
/// on the kernel's own server: the grant was Georg's yes for all of them, installs included).
pub fn offer_for(tool: &str, granted: bool) -> Offer {
    if granted { Offer::Gate } else { offer(tool) }
}

/// MC-10: a service's tool for a client; with a live grant every registered tool is relayed.
pub fn service_offer_for(safety: Safety, verified: bool, granted: bool) -> Option<Offer> {
    if granted { Some(Offer::Gate) } else { service_offer(safety, verified) }
}

/// The reason for a kernel tool the table names as dropped (None: offered, or a name the table does not know,
/// which the service answers as an unknown tool).
pub fn known_dropped(tool: &str) -> Option<&'static str> {
    match named(tool)? {
        Offer::Dropped(why) => Some(why),
        _ => None,
    }
}

fn named(tool: &str) -> Option<Offer> {
    let is = |set: &[&str]| set.contains(&tool);
    let o = if is(GATE) {
        Offer::Gate
    } else if is(INSTALLER) {
        Offer::Installer
    } else if is(RAW) {
        Offer::Dropped(WHY_RAW)
    } else if is(PROCESSES) {
        Offer::Dropped(WHY_PROCESSES)
    } else if is(FILES) {
        Offer::Dropped(WHY_FILES)
    } else if is(CONSOLE) {
        Offer::Dropped(WHY_CONSOLE)
    } else if is(DEVICES) {
        Offer::Dropped(WHY_DEVICES)
    } else if is(OTHER) {
        Offer::Dropped(WHY_OTHER)
    } else {
        return None;
    };
    Some(o)
}

/// MC-9a: what the gate does with a service's tool. A root service's looking tool is a call; its changing tools
/// (settingsd's `settings_set`, `settings_undo`: the guarded settings) go to the installer, which asks first; a
/// tool of any other uid only claims its class (MC-6) and is not offered.
pub fn service_offer(safety: Safety, verified: bool) -> Option<Offer> {
    match (verified, safety) {
        (false, _) => None,
        (true, Safety::Read) => Some(Offer::Gate),
        (true, _) => Some(Offer::Installer),
    }
}

/// The answer for a tool the service does not offer.
pub fn dropped_text(tool: &str, why: &str) -> String {
    format!("{tool} is not offered by the user-space MCP service (bitsmcp): {why}. The kernel's own MCP server has it (CONFIG.TXT mcp=kernel or mcp=both)")
}

/// The answer for an installer request the kernel refused before anyone was asked.
pub fn installer_text(tool: &str, errno: i32) -> String {
    let what = if errno == 1 { String::from("EPERM") } else { format!("errno {errno}") };
    format!("{tool} goes to the installer bitsinstall, which asks a person at the machine first (MC-9); no installer is running, the kernel answered {what}. The kernel's own MCP server has it (CONFIG.TXT mcp=kernel or mcp=both)")
}

/// `stop`/`output` through the gate reach only the gate's own calls, never a program by pid or pgid.
pub fn process_target(tool: &str, args: &Json) -> Option<&'static str> {
    let named = |k: &str| args.get(k).is_some_and(|v| !v.is_null());
    ((tool == "stop" || tool == "output") && (named("pid") || named("pgid"))).then_some("a program by pid or pgid is not reached through the user-space MCP service (MC-8a); call names this service's own calls")
}

// ---------------------------------------------------------------------------------------------------------
// Commands.

/// One command as the kernel reads it.
#[derive(Debug, PartialEq)]
pub enum Command {
    Hello,
    /// The client's key ("" for none).
    List { client: String },
    /// MC-10: one client's full-access state.
    Access { client: String },
    /// MC-10: the installer's full-access table.
    Grants(Vec<crate::access::Grant>),
    Auth { peer: String, token: Vec<u8> },
    Call(Request),
    Install(Request),
    Answered,
    /// MC-9a, the installer's session (`crate::installer`).
    Claim,
    Next { wait_ms: u64 },
    Decide(crate::installer::Decision),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub tool: String,
    pub args: Json,
    pub timeout_ms: u64,
    /// The MCP client's address (`a.b.c.d:port`), for the event.
    pub peer: String,
    /// MC-10: the SHA-256 of the client's TLS certificate (`access::is_client`), "" for a plain connection.
    pub client: String,
}

/// Peer text kept: an IPv4 address and port, printable.
const PEER_MAX: usize = 32;

fn peer_text(s: &str) -> String {
    s.chars().take(PEER_MAX).map(|c| if c.is_ascii_graphic() { c } else { '?' }).collect()
}

pub fn encode_hello() -> Vec<u8> {
    b"hello".to_vec()
}
pub fn encode_list() -> Vec<u8> {
    b"list".to_vec()
}
/// MC-10: the list for `client` (a granted one gets every tool).
pub fn encode_list_for(client: &str) -> Vec<u8> {
    if client.is_empty() { encode_list() } else { format!("list {client}").into_bytes() }
}
pub fn encode_answered() -> Vec<u8> {
    b"answered".to_vec()
}
pub fn encode_auth(peer: &str, token: &[u8]) -> Vec<u8> {
    let mut v = format!("auth {} ", peer_text(peer)).into_bytes();
    v.extend_from_slice(token);
    v
}

fn encode_request(word: &str, r: &Request) -> Vec<u8> {
    let mut s = format!("{word} {{\"tool\":");
    json::escape_into(&r.tool, &mut s);
    s.push_str(",\"arguments\":");
    s.push_str(&r.args.to_string());
    s.push_str(&format!(",\"timeout_ms\":{},\"peer\":", r.timeout_ms));
    json::escape_into(&peer_text(&r.peer), &mut s);
    if !r.client.is_empty() {
        s.push_str(&format!(",\"client\":\"{}\"", r.client));
    }
    s.push('}');
    s.into_bytes()
}
pub fn encode_call(r: &Request) -> Vec<u8> {
    encode_request("call", r)
}
pub fn encode_install(r: &Request) -> Vec<u8> {
    encode_request("install", r)
}

fn parse_request(rest: &[u8]) -> Option<Request> {
    let j = Json::parse(core::str::from_utf8(rest).ok()?).ok()?;
    let tool = String::from(j.get("tool")?.as_str()?);
    if tool.is_empty() || tool.len() > 128 || tool.chars().any(char::is_control) {
        return None;
    }
    let args = match j.get("arguments") {
        None | Some(Json::Null) => Json::Obj(Vec::new()),
        Some(a @ Json::Obj(_)) => a.clone(),
        Some(_) => return None,
    };
    let timeout_ms = match j.get("timeout_ms") {
        None | Some(Json::Null) => rpc::DEFAULT_TIMEOUT_MS,
        Some(v) => v.as_u64().filter(|ms| (1..=rpc::MAX_TIMEOUT_MS).contains(ms))?,
    };
    let peer = peer_text(j.get("peer").and_then(Json::as_str).unwrap_or("-"));
    let client = match j.get("client") {
        None | Some(Json::Null) => String::new(),
        Some(c) => String::from(c.as_str().filter(|c| crate::access::is_client(c))?),
    };
    Some(Request { tool, args, timeout_ms, peer, client })
}

/// A command line (one write, a trailing newline allowed). None: EINVAL.
pub fn parse_command(line: &[u8]) -> Option<Command> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    if line.len() > LINE_MAX {
        return None;
    }
    let (word, rest) = match line.iter().position(|&b| b == b' ') {
        Some(i) => (&line[..i], &line[i + 1..]),
        None => (line, &[][..]),
    };
    match word {
        b"hello" if rest.is_empty() => Some(Command::Hello),
        b"list" if rest.is_empty() => Some(Command::List { client: String::new() }),
        b"list" => client_word(rest).map(|client| Command::List { client }),
        b"access" => client_word(rest).map(|client| Command::Access { client }),
        b"grants" => crate::access::parse_table(rest).map(Command::Grants),
        b"answered" if rest.is_empty() => Some(Command::Answered),
        b"auth" => {
            let i = rest.iter().position(|&b| b == b' ')?;
            let peer = core::str::from_utf8(&rest[..i]).ok()?;
            let token = &rest[i + 1..];
            if token.is_empty() || token.contains(&b'\n') {
                return None;
            }
            Some(Command::Auth { peer: peer_text(peer), token: token.to_vec() })
        }
        b"call" => parse_request(rest).map(Command::Call),
        b"install" => parse_request(rest).map(Command::Install),
        b"claim" if rest.is_empty() => Some(Command::Claim),
        b"next" => crate::installer::parse_next(rest).map(|wait_ms| Command::Next { wait_ms }),
        b"decide" => crate::installer::parse_decide(rest).map(Command::Decide),
        _ => None,
    }
}

fn client_word(rest: &[u8]) -> Option<String> {
    let c = core::str::from_utf8(rest).ok()?;
    crate::access::is_client(c).then(|| String::from(c))
}

// ---------------------------------------------------------------------------------------------------------
// Answers.

/// `hello`'s answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub mode: Mode,
    pub port: u16,
    pub conns: usize,
    pub version: String,
}

pub fn encode_hello_answer(h: &Hello) -> String {
    let mut s = format!("{{\"mode\":\"{}\",\"port\":{},\"conns\":{},\"version\":", h.mode.name(), h.port, h.conns);
    json::escape_into(&h.version, &mut s);
    s.push('}');
    s
}

pub fn decode_hello_answer(text: &str) -> Option<Hello> {
    let j = Json::parse(text).ok()?;
    let mode = Mode::parse(j.get("mode")?.as_str()?)?;
    let port = u16::try_from(j.get("port")?.as_u64()?).ok().filter(|&p| p != 0)?;
    let conns = usize::try_from(j.get("conns")?.as_u64()?).ok()?;
    let version = String::from(j.get("version").and_then(Json::as_str).unwrap_or(""));
    Some(Hello { mode, port, conns, version })
}

/// One tool of `list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    pub description: String,
    /// The schema as the kernel's own `tools/list` sends it (text, byte for byte).
    pub schema: String,
    pub owner: String,
    pub safety: Safety,
    pub verified: bool,
    pub pid: Option<u32>,
    /// Registered by a user-space service.
    pub service: bool,
    pub installer: bool,
}

impl rpc::ToolMeta for Listed {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn input_schema(&self) -> &str {
        &self.schema
    }
    fn owner(&self) -> &str {
        &self.owner
    }
    fn safety(&self) -> Safety {
        self.safety
    }
    fn service_pid(&self) -> Option<u32> {
        self.pid
    }
    fn safety_verified(&self) -> bool {
        self.verified
    }
    fn is_service(&self) -> bool {
        self.service
    }
}

pub fn encode_list_answer(tools: &[Listed]) -> String {
    let mut s = String::from("{\"tools\":[");
    for (i, t) in tools.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let field = |s: &mut String, k: &str, v: &str| {
            s.push_str(&format!("\"{k}\":"));
            json::escape_into(v, s);
            s.push(',');
        };
        s.push('{');
        field(&mut s, "name", &t.name);
        field(&mut s, "description", &t.description);
        field(&mut s, "schema", &t.schema);
        field(&mut s, "owner", &t.owner);
        field(&mut s, "safety", rpc::safety_name(t.safety));
        let pid = t.pid.map_or(String::from("null"), |p| format!("{p}"));
        s.push_str(&format!("\"verified\":{},\"pid\":{pid},\"service\":{},\"offer\":\"{}\"}}", t.verified, t.service, if t.installer { "installer" } else { "gate" }));
    }
    s.push_str("]}");
    s
}

pub fn decode_list_answer(text: &str) -> Option<Vec<Listed>> {
    let j = Json::parse(text).ok()?;
    let mut v = Vec::new();
    for t in j.get("tools")?.as_array()? {
        let s = |k: &str| t.get(k).and_then(Json::as_str).map(String::from);
        v.push(Listed {
            name: s("name")?,
            description: s("description")?,
            schema: s("schema")?,
            owner: s("owner")?,
            safety: rpc::safety_from(t.get("safety")?.as_str()?)?,
            verified: t.get("verified")?.as_bool()?,
            pid: t.get("pid").and_then(Json::as_u64).and_then(|p| u32::try_from(p).ok()),
            service: t.get("service").and_then(Json::as_bool).unwrap_or(false),
            installer: t.get("offer").and_then(Json::as_str) == Some("installer"),
        });
    }
    Some(v)
}

/// A call's result: Ok(the tool's result) or Err(the text of an `isError` answer such as "Still running").
pub fn encode_result(r: &Result<ToolResult, String>) -> String {
    let one = |k: &str, text: &str| {
        let mut s = format!("{{\"{k}\":");
        json::escape_into(text, &mut s);
        s.push('}');
        s
    };
    match r {
        Err(still) => one("still", still),
        Ok(Err(ToolError::BadArgument(w))) => one("bad_argument", w),
        Ok(Err(ToolError::Refused(w))) => one("refused", w),
        Ok(Err(ToolError::Failed(w))) => one("failed", w),
        Ok(Ok(out)) => {
            let mut s = String::from("{\"ok\":[");
            for (i, o) in out.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                match o {
                    ToolOutput::Text(t) => {
                        s.push_str("{\"text\":");
                        json::escape_into(t, &mut s);
                        s.push('}');
                    }
                    ToolOutput::Png(p) => {
                        s.push_str("{\"png\":\"");
                        b64::encode_into(p, &mut s);
                        s.push_str("\"}");
                    }
                    ToolOutput::Json(j) => {
                        s.push_str("{\"json\":");
                        s.push_str(&j.to_string());
                        s.push('}');
                    }
                }
            }
            s.push_str("]}");
            s
        }
    }
}

pub fn decode_result(text: &str) -> Option<Result<ToolResult, String>> {
    let j = Json::parse(text).ok()?;
    let s = |k: &str| j.get(k).and_then(Json::as_str).map(String::from);
    if let Some(t) = s("still") {
        return Some(Err(t));
    }
    if let Some(t) = s("bad_argument") {
        return Some(Ok(Err(ToolError::BadArgument(t))));
    }
    if let Some(t) = s("refused") {
        return Some(Ok(Err(ToolError::Refused(t))));
    }
    if let Some(t) = s("failed") {
        return Some(Ok(Err(ToolError::Failed(t))));
    }
    let mut out = Vec::new();
    for o in j.get("ok")?.as_array()? {
        if let Some(t) = o.get("text").and_then(Json::as_str) {
            out.push(ToolOutput::Text(String::from(t)));
        } else if let Some(p) = o.get("png").and_then(Json::as_str) {
            out.push(ToolOutput::Png(b64::decode(p)?));
        } else if let Some(v) = o.get("json") {
            out.push(ToolOutput::Json(v.clone()));
        } else {
            return None;
        }
    }
    Some(Ok(Ok(out)))
}

#[cfg(test)]
mod tests;
