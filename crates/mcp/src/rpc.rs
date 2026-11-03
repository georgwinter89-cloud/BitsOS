//! JSON-RPC 2.0 and the MCP methods for the kernel's server: `mcp_wire::rpc` (shared with the user-space service
//! bitsmcp, MC-8a) over the kernel's catalog, with the kernel's events (B6) and `policy::check` (B3) around a call.

use crate::catalog::{Catalog, Tool};
use crate::{Json, Safety, ToolContext, ToolEntry};
use alloc::string::String;
use mcp_wire::rpc as wire;

pub use mcp_wire::rpc::{ANSWER_MAX, Answer, DEFAULT_TIMEOUT_MS, MAX_TIMEOUT_MS, PROTOCOL, escaped_len, id_ok, safety_name};
#[cfg(test)]
use crate::{ToolError, ToolOutput, json};
#[cfg(test)]
use alloc::format;
#[cfg(test)]
use mcp_wire::rpc::TOO_BIG;

const INSTRUCTIONS: &str = "BitsOS V2 is Georg's own operating system: a Rust kernel that runs Linux x86-64 programs. \
This MCP server runs inside the kernel, independent of user space. tools/list names each tool with its owner (the \
lane that built it) and safety class (read, change, delete_overwrite) in its annotations. Calls take an optional \
timeout_ms (1 to 86400000, default 10000); a call still running then answers 'Still running ... (call n)', \
and output shows its progress and, once it ends, its result; stop asks it to end. reboot answers before it \
restarts. There is no poweroff tool. Tools whose owner is a lower-case service name come from BitsOS's user-space \
services (annotation service_pid): they appear and vanish with their service (ask tools/list again when one is \
missing), and a call to one that passes its timeout_ms is cancelled at the service and answers with an error.";

const INFO: wire::Info<'static> = wire::Info { version: env!("CARGO_PKG_VERSION"), instructions: INSTRUCTIONS };

/// Runs a tool's handler for `tools/call`: the server hands it to a worker thread and waits until the call's
/// deadline. `Err` is the text of an `isError` result (e.g. "Still running ...").
pub type Runner<'a> = dyn FnMut(&Tool, ToolContext, &Json) -> Result<crate::ToolResult, String> + 'a;

impl wire::ToolMeta for Tool {
    fn name(&self) -> &str {
        Tool::name(self)
    }
    fn description(&self) -> &str {
        Tool::description(self)
    }
    fn input_schema(&self) -> &str {
        Tool::input_schema(self)
    }
    fn owner(&self) -> &str {
        Tool::owner(self)
    }
    fn safety(&self) -> Safety {
        Tool::safety(self)
    }
    fn service_pid(&self) -> Option<u32> {
        Tool::service_pid(self)
    }
    fn safety_verified(&self) -> bool {
        Tool::safety_verified(self)
    }
    fn is_service(&self) -> bool {
        Tool::is_service(self)
    }
}

impl wire::ToolSet for Catalog<'_> {
    type Tool = Tool;
    fn find(&self, name: &str) -> Option<Tool> {
        Catalog::find(self, name)
    }
    fn sorted(&self) -> alloc::vec::Vec<Tool> {
        Catalog::sorted(self)
    }
}

/// The kernel around a call: events and the policy hook.
pub(crate) struct KernelHost;

impl wire::Host for KernelHost {
    fn bad_schema(&self, tool: &str, owner: &str) {
        klog::event!(Error, "mcp", "tool.bad_schema", tool = tool, owner = owner);
    }
    fn answer_cut(&self, bytes: usize, limit: usize) {
        klog::event!(Info, "mcp", "answer.cut", bytes = bytes, limit = limit);
    }
    fn check(&self, tool: &str) -> Result<(), String> {
        policy_check(tool)
    }
    fn tool_failed(&self, tool: &str) {
        klog::event!(Debug, "mcp", "tool.failed", tool = tool);
    }
}

/// B3 before a call runs (MCP clients act as BitsOS itself for now; per-client identity comes with PM).
pub(crate) fn policy_check(tool: &str) -> Result<(), String> {
    policy::check(&policy::Subject::KERNEL, &policy::Access::McpAction { tool }).map_err(|d| {
        klog::event!(Info, "mcp", "tool.refused", tool = tool, reason = d.reason.as_str());
        d.reason
    })
}

/// Handles one request body, running tools right here in the calling thread (unit tests, tools that cannot
/// block). The server uses `handle_with`.
pub fn handle(body: &[u8], now_ns: u64, tools: &'static [ToolEntry]) -> Answer {
    handle_with(body, now_ns, &Catalog::of(tools), &mut |tool, ctx, args| Ok(tool.invoke(&ctx, args)))
}

/// Handles one request body. `now_ns` starts a call's deadline; `tools` is the built-in registry plus the
/// services' tools (sorted here); `run` runs a tool's handler.
pub fn handle_with(body: &[u8], now_ns: u64, tools: &Catalog<'_>, run: &mut Runner<'_>) -> Answer {
    wire::handle_with(body, now_ns, tools, &KernelHost, &INFO, &mut |tool, deadline_ns, args| run(tool, ToolContext { caller: policy::Subject::KERNEL, deadline_ns }, args))
}

#[cfg(test)]
mod tests;
