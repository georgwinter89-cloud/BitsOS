//! FS-13: the start-up preload from MCP and the console: `preload` shows the status line
//! (`/sys/bitsos/preload_status`), `preload clear` / `{"clear":true}` empties the record (as writing 1 to
//! `/sys/bitsos/preload_clear`). Switching it on or off and the budget are settings (settingsd
//! `kernel.preload.enabled`, `kernel.preload.max_mb`), not this tool's.

use alloc::string::String;
use alloc::vec;
use mcp::{Json, ToolContext, ToolError, ToolOutput, ToolResult, mcp_tool};

fn preload(_ctx: &ToolContext, args: &Json) -> ToolResult {
    let clear = match args.get("clear") {
        None | Some(Json::Null) => false,
        Some(Json::Bool(b)) => *b,
        Some(_) => return Err(ToolError::BadArgument(String::from("clear must be true or false"))),
    };
    let mut out = String::new();
    if clear {
        crate::preload::request_clear("mcp");
        out.push_str("record cleared (the empty record is stored within about a second)\n");
    }
    out.push_str(&crate::preload::status_line());
    Ok(vec![ToolOutput::Text(out)])
}

mcp_tool!(
    name: "preload",
    owner: "FS",
    safety: Change,
    description: "The start-up preload (FS-13): which file pages programs read in their first 20 s is recorded per program in /data/system/preload/record, and after the next boot a background task reads them into the page cache, most-used programs first. Without arguments: one status line (state, enabled, max_mb (all = no cap: the pass stops when free memory falls to floor_mb, a quarter of RAM; preloaded pages nobody used are the first that reclaim gives back), this boot's pass: programs, files, mb, ms, stopped=end|max_mb|memory|reclaim|off|cleared, yields; record_programs, record_mb, sessions recording now; stale and renewed files of the pass; unknown/unknown_pages, dropped/dropped_pages, other_pages, kthread_pages: what was read while a session ran but no record holds; floor_mb). clear:true empties the record first (nothing is preloaded after the next boot until programs ran again). On/off and the budget are settings: kernel.preload.enabled, kernel.preload.max_mb.",
    schema: r#"{"type":"object","properties":{"clear":{"type":"boolean","description":"empty the record first"}}}"#,
    handler: preload,
);

fn preload_cmd(c: &mut console::monitor::Ctx) -> Result<(), String> {
    match c.args.first().copied() {
        None => {}
        Some("clear") => {
            crate::preload::request_clear("console");
            c.println("record cleared");
        }
        Some(_) => return Err(String::from("usage: preload [clear]")),
    }
    let line = crate::preload::status_line();
    c.println(&line);
    Ok(())
}
console::command!(name: "preload", args: "[clear]", help: "start-up preload status; clear empties its record", physical_only: false, run: preload_cmd);
