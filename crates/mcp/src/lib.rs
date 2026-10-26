//! The MCP kernel service (owner: MC; registry contract `mcp_tool!` from G0, V2-PLAN §6, B5).
//!
//! Tools are registered by the lane that owns their back end, in its own crate, with an owner and a safety
//! class (B5): `read` (looks only), `change` (changes state), `delete_overwrite` (destroys or replaces data;
//! the policy hook asks Georg by default once PM-1 lands). Every call passes `policy::check` (B3) first.
//!
//! MC-1: the HTTP/1.1 + JSON-RPC server on `net::KSocket` (`server`, `conn`, `http`, `rpc`); the kernel starts
//! it with `start_kernel` (kernel.rs), host tests with `server::start` over `net::host::StdSockets` and
//! `host::StdEnv`.
//! MC-2: the tool framework (`calls`, `worker`): handlers run in worker threads (class Normal, never KernelRt),
//! waited for until the call's deadline; after it the caller gets "Still running ... (call n)" and the call's
//! progress text and result wait for `output`, `stop` asks it to end (both answered by the server itself);
//! `ToolContext::after_answer` runs an action only after the answer is out (reboot, reload). The generic tools
//! live in `tools` (`hw_info`, `net_info`, `ktest`, `cpus`, `reboot`, `msr_read`, `read_phys`); `boot` keeps
//! what BootInfo says about the running generation (`set_boot_info`). There is no power-off tool (R8f).
//! MC-3: `screen` (PNG from the display's RAM frame through `png` + `deflate`, scale/region; the console's text
//! grid) and `send_keys`/`send_mouse` (events into `console::evdev`, always flagged `injected`, R8c) in `tools`.
//! MC-6: tools registered by user-space services through `/run/bitsos/mcp.sock` (`svc`), listed and called next
//! to the built-in ones (`catalog`).
#![cfg_attr(target_os = "none", no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod b64;
mod deflate;
mod boot;
pub mod bounded;
mod calls;
pub mod catalog;
mod conn;
#[cfg(not(target_os = "none"))]
pub mod host;
pub mod http;
pub mod json;
mod kernel;
pub mod png;
pub mod rpc;
mod schema;
pub mod server;
/// MC-push: SHA-256 for `manifest`.
pub mod sha256;
pub mod svc;
pub mod testclient;
pub mod tools;
mod worker;

pub use boot::{BootFacts, facts as boot_facts, reason_name, running_reason_with, set_boot_info, set_running_kernel_id};
pub use calls::{AfterAnswer, DEFAULT_GRACE_MS, KEEP_NS, MAX_GRACE_MS, OUTPUT_KEEP, ProcessTools, StopTarget, set_process_tools};
pub use kernel::{KernelEnv, start_kernel};
pub use server::{Env, ServerConfig, ServerHandle, SpawnFailed, StartError, ThreadKind, start, start_with};
pub use svc::Registry;

use alloc::string::String;
use alloc::vec::Vec;
pub use json::Json;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Safety {
    Read,
    Change,
    DeleteOverwrite,
}

/// What a tool call knows about itself. Its methods (`calls.rs`) reach the call behind it: `output` (progress
/// text for the `output` tool), `stop_requested`, `expired`, `remaining_ns`, `after_answer`, `call_id`.
pub struct ToolContext {
    /// Who calls (MCP clients act as BitsOS itself for now; per-client identity comes with MC-6/PM).
    pub caller: policy::Subject,
    /// Absolute monotonic deadline (ns). The handler runs in a worker thread; past the deadline (+ 0.25 s) the
    /// caller gets "Still running (call n)" and the result waits for `output`; a tool that loops should look at
    /// `stop_requested` (B4).
    pub deadline_ns: u64,
}

#[derive(Clone, Debug)]
pub enum ToolOutput {
    Text(String),
    /// PNG bytes (the server base64-encodes them as MCP image content).
    Png(Vec<u8>),
    Json(Json),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolError {
    /// A bad or missing argument (JSON-RPC -32602 with the argument's name).
    BadArgument(String),
    /// Refused by a safety rule or Georg's policy.
    Refused(String),
    Failed(String),
}

pub type ToolResult = Result<Vec<ToolOutput>, ToolError>;

pub struct ToolEntry {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON schema of the arguments, as text (sent unchanged in tools/list).
    pub input_schema: &'static str,
    /// Lane or service that owns the back end (B5), e.g. "RB", "PC", "kernel".
    pub owner: &'static str,
    pub safety: Safety,
    pub handler: fn(&ToolContext, &Json) -> ToolResult,
}

kreg::declare!(pub TOOLS: ToolEntry = "mcptool");

#[doc(hidden)]
pub use kreg as __kreg;

/// `mcp::mcp_tool!(name: "log", owner: "RB", safety: Read, description: "...", schema: "{...}", handler: f);`
#[macro_export]
macro_rules! mcp_tool {
    (name: $name:literal, owner: $owner:literal, safety: $safety:ident, description: $desc:literal, schema: $schema:literal, handler: $h:path $(,)?) => {
        $crate::__kreg::register!("mcptool", $crate::ToolEntry, $crate::ToolEntry {
            name: $name, description: $desc, input_schema: $schema, owner: $owner, safety: $crate::Safety::$safety, handler: $h,
        });
    };
}

pub fn find(name: &str) -> Option<&'static ToolEntry> {
    TOOLS.entries().iter().find(|t| t.name == name)
}

/// Every tool name unique, every schema valid JSON (the registry contract).
fn registry_ok() -> ktest::TestResult {
    let mut names: Vec<&str> = TOOLS.entries().iter().map(|t| t.name).collect();
    names.sort();
    for w in names.windows(2) {
        ktest::kassert!(w[0] != w[1], "duplicate MCP tool {}", w[0]);
    }
    for t in TOOLS.entries() {
        ktest::kassert!(Json::parse(t.input_schema).is_ok(), "tool {}: schema is not JSON", t.name);
        ktest::kassert!(!t.owner.is_empty(), "tool {} has no owner", t.name);
    }
    Ok(())
}
ktest::ktest!("kreg.mcp_tools_unique", Safe, registry_ok);

/// A built-in tool so the registry is never empty: answers with the kernel's name and version, and on a second
/// line which start this is (Reload counter, generation, boot id), so a host tool sees that a Reload took
/// (`host/mcp/reload.mjs` compares the answer before and after).
fn version(_ctx: &ToolContext, _args: &Json) -> ToolResult {
    let mut t = String::from(concat!("BitsOS V2 kernel ", env!("CARGO_PKG_VERSION")));
    t.push_str(&version_start_line(boot_facts()));
    Ok(alloc::vec![ToolOutput::Text(t)])
}

fn version_start_line(f: Option<BootFacts>) -> String {
    use core::fmt::Write;
    let mut t = String::from("\nstart:");
    let Some(f) = f else {
        t.push_str(" unknown (no BootInfo handed over)");
        return t;
    };
    match f.reload {
        Some(n) => {
            let _ = write!(t, " reload {n}");
        }
        None => t.push_str(" reload unknown"),
    }
    match f.generation {
        Some(g) => {
            let _ = write!(t, ", generation {}", g.number);
        }
        None => t.push_str(", generation unknown"),
    }
    if let Some(id) = f.boot_id {
        t.push_str(", boot id ");
        for b in id {
            let _ = write!(t, "{b:02x}");
        }
    }
    t
}
mcp_tool!(
    name: "version",
    owner: "MC",
    safety: Read,
    description: "Kernel name and version; the second line names this start (Reload counter, generation, boot id).",
    schema: r#"{"type":"object","properties":{}}"#,
    handler: version,
);

/// `output` and `stop` are answered by the server itself (`calls.rs`); these handlers only run when a handler is
/// called outside a server.
pub(crate) fn server_only(_ctx: &ToolContext, _args: &Json) -> ToolResult {
    Err(ToolError::Failed(String::from("answered by the MCP server itself; no server is handling this call")))
}
mcp_tool!(
    name: "output",
    owner: "MC",
    safety: Read,
    description: "Progress and state of a tool call that is still running (its caller got 'Still running ... (call n)'), and its result once it ended. call: its number; or pid: the program a run call started (its new stdout/stderr); a pid no call runs (a console job) is described with its state (its output goes to its terminal, not here). Without call or pid: the newest one. Answered by the server itself, also while every worker is busy.",
    schema: r#"{"type":"object","properties":{"call":{"type":"integer","minimum":1,"description":"the call number from the Still running answer"},"pid":{"type":"integer","minimum":1,"description":"a program's pid (from run or ps)"}}}"#,
    handler: server_only,
);
mcp_tool!(
    name: "stop",
    owner: "MC",
    safety: Change,
    description: "Stops a running tool call (call: its number; without call or pid: the newest running one) and answers with its result if it ended. A program named by pid (a run call's, or a console job's) gets SIGINT, then SIGKILL after grace_ms (default 1000; 0 = SIGKILL at once); pgid stops a whole process group the same way; a run call named by call (or none) gets SIGKILL at once unless grace_ms is given. PID 1 only with confirm: true; kernel threads never. Kernel tools end at their next look at the stop flag; one that never looks runs on, and the answer says so.",
    schema: r#"{"type":"object","properties":{"call":{"type":"integer","minimum":1,"description":"the call number from the Still running answer"},"pid":{"type":"integer","minimum":1,"description":"a program's pid (from run or ps)"},"pgid":{"type":"integer","minimum":1,"description":"a process group (a console job)"},"grace_ms":{"type":"integer","minimum":0,"maximum":10000,"description":"between SIGINT and SIGKILL (default 1000 with pid or pgid, 0 with call)"},"confirm":{"type":"boolean","description":"needed to stop PID 1 (init)"}}}"#,
    handler: server_only,
);
