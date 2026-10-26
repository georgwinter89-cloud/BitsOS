//! MCP tool `snd` (SN-10, Change): without `output` the status (`output::status_lines`); with `output` it switches
//! first (`output::apply`, the same path as `/sys/bitsos/snd_output`) and then shows the status. The tool never
//! plays sound itself, so its own ktests are Safe.

use crate::output::{self, Why};
use alloc::vec;
use mcp::{Json, ToolContext, ToolError, ToolOutput, ToolResult, mcp_tool};

fn snd(_ctx: &ToolContext, args: &Json) -> ToolResult {
    let done = match args.get("output") {
        None | Some(Json::Null) => None,
        Some(Json::Str(v)) => {
            let r = output::parse_request(v)
                .ok_or_else(|| ToolError::BadArgument(alloc::format!("output {v:?} is not {}", output::VALUES)))?;
            Some(output::apply(&r, "mcp").map_err(|e| match e.why {
                Why::BadValue => ToolError::BadArgument(e.text),
                Why::NoHdmi => ToolError::Refused(e.text),
                Why::Failed => ToolError::Failed(e.text),
            })?)
        }
        Some(_) => return Err(ToolError::BadArgument(alloc::format!("output must be text: {}", output::VALUES))),
    };
    let mut lines = output::status_lines();
    if let Some(d) = done {
        lines.insert(0, d);
    }
    Ok(vec![ToolOutput::Text(lines.join("\n"))])
}

mcp_tool!(
    name: "snd",
    owner: "SN",
    safety: Change,
    description: "Where the kernel's sound plays. Without arguments: the target (auto, jack, hdmi, usb), the mixer (playing endpoint, counters), HDMI (display side on/off/absent, GPU controller streaming), every output with its plug state (* = playing now) and every client (format, state, volume, underruns). output: switches first: auto follows plug state; jack, hdmi, usb prefer that port (else auto); hdmi turns HDMI sound on (writes the display side's audio registers; refused without a display engine, e.g. in QEMU); hdmi-off turns HDMI sound off (back to auto first). The same as writing /sys/bitsos/snd_output.",
    schema: r#"{"type":"object","properties":{"output":{"type":"string","enum":["auto","jack","hdmi","usb","hdmi-off"],"description":"switch the output first"}}}"#,
    handler: snd,
);
