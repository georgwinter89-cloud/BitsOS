//! `idle_mode` (FD-1 measuring build only, `--cfg bitsos_fd_measure`): switches the idle loop's sleep depth at
//! run time for A/B runs on one boot (auto, c1 = C1 only as before FD-1, forced = always the shallowest deep
//! state) and the residency factor of `auto`. Never in a release build.

use alloc::format;
use alloc::string::String;
use mcp::{Json, ToolContext, ToolError, ToolOutput, ToolResult};
use sched::cstate::{self, Mode};

fn tool(_ctx: &ToolContext, args: &Json) -> ToolResult {
    let bad = |n: &str| ToolError::BadArgument(String::from(n));
    if let Some(v) = args.get("mode").filter(|v| !v.is_null()) {
        let m = match v.as_str().ok_or_else(|| bad("mode"))? {
            "auto" => Mode::Auto,
            "c1" => Mode::C1,
            "forced" => Mode::Forced,
            _ => return Err(bad("mode")),
        };
        cstate::set_mode(m);
    }
    if let Some(v) = args.get("factor").filter(|v| !v.is_null()) {
        cstate::set_residency_factor(v.as_u64().filter(|f| (1..=1000).contains(f)).ok_or_else(|| bad("factor"))? as u32);
    }
    Ok(alloc::vec![ToolOutput::Text(format!("{}\n", crate::sched_tool::idle_line()))])
}

mcp::mcp_tool!(
    name: "idle_mode",
    owner: "SC",
    safety: Change,
    description: "FD-1 measuring build: the idle loop's sleep depth. mode: auto (deep C-state when the next timer and the predicted idle allow), c1 (C1 only, as before FD-1; CONFIG idle=c1), forced (always the shallowest deep state); factor: auto's target residency in multiples of the exit latency. Without arguments: the report.",
    schema: r#"{"type":"object","properties":{"mode":{"type":"string","enum":["auto","c1","forced"]},"factor":{"type":"integer","minimum":1,"maximum":1000}}}"#,
    handler: tool,
);
