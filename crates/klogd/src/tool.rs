//! MCP tool `log` (owner RB, safety Read; spec/net.md §5.2: "`log` = kernel ring buffer with lines/since").
//!
//! Answer: a head line `log next=<seq> oldest=<seq> shown=<n> missed=<n>`, then one line per record
//! `#<seq> [time] L subsystem name fields | message`, with `[... n records lost]` where the sequence has a hole.
//! A client follows the log by passing `next` as `since` in its next call. Text from devices and the network is
//! escaped by klog's renderer, so every record is exactly one line and no record line can be forged. A ring
//! held too long gives a short answer marked `busy` instead of a wait (the read is bounded, B4).

use alloc::format;
use alloc::string::String;
use core::fmt::Write;
use klog::{Filter, Level, Page};
use mcp::{Json, ToolContext, ToolError, ToolOutput, ToolResult};

/// Lines when `max` is not given.
const DEFAULT_MAX: u64 = 200;
/// Lines per answer at most: a record line is at most `klog::LINE_BYTES`, MCP answers stay well below 4 MB.
pub const MAX_LINES: u64 = 2000;

pub(crate) fn bad(name: &str) -> ToolError {
    ToolError::BadArgument(String::from(name))
}

pub(crate) fn opt_u64(args: &Json, key: &str) -> Result<Option<u64>, ToolError> {
    match args.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(v) => v.as_u64().map(Some).ok_or_else(|| bad(key)),
    }
}

pub(crate) fn opt_level(args: &Json) -> Result<Option<Level>, ToolError> {
    match opt_str(args, "level")? {
        None => Ok(None),
        Some(l) => Ok(Some(Level::parse(l).ok_or_else(|| bad("level"))?)),
    }
}

pub(crate) fn opt_str<'a>(args: &'a Json, key: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(v) => v.as_str().filter(|s| !s.is_empty()).map(Some).ok_or_else(|| bad(key)),
    }
}

pub fn render(page: &Page) -> String {
    let mut out = String::new();
    let _ = write!(out, "log next={} oldest={} shown={} missed={}", page.next, page.oldest, page.records.len(), page.missed());
    if page.ahead {
        out.push_str(" (since is past the newest record: the kernel restarted; continue with next)");
    }
    if page.busy {
        out.push_str(" busy (the log was held too long; this answer may be short: call again with since=next)");
    }
    let mut gaps = page.gaps.iter().peekable();
    for r in &page.records {
        while let Some(g) = gaps.next_if(|g| g.before <= r.seq) {
            let _ = write!(out, "\n[... {} records lost]", g.count);
        }
        let _ = write!(out, "\n#{} {}", r.seq, r.render());
    }
    for g in gaps {
        let _ = write!(out, "\n[... {} records lost]", g.count);
    }
    out
}

pub fn log(_ctx: &ToolContext, args: &Json) -> ToolResult {
    if !matches!(args, Json::Obj(_) | Json::Null) {
        return Err(bad("arguments"));
    }
    let since = opt_u64(args, "since")?;
    let max = match opt_u64(args, "max")? {
        None => DEFAULT_MAX,
        Some(m) if (1..=MAX_LINES).contains(&m) => m,
        Some(_) => return Err(bad("max")),
    } as usize;
    let level = opt_level(args)?;
    let f = Filter { level, subsystem: opt_str(args, "subsystem")?, name: opt_str(args, "name")? };
    let page = match since {
        Some(s) => klog::read(s, max, &f),
        None => klog::read_tail(max, &f),
    };
    Ok(alloc::vec![ToolOutput::Text(render(&page))])
}

mcp::mcp_tool!(
    name: "log",
    owner: "RB",
    safety: Read,
    description: "Kernel log (structured events) with sequence numbers. Without since: the newest max records. With since: the records from that sequence number on; pass the answer's next= as since to follow the log. '[... n records lost]' marks records overwritten before they were read. Filters: level (most verbose level to include: error, warn, info, debug, trace), subsystem (also matches its dotted sub-subsystems, e.g. net matches net.dhcp), name (prefix of the event name, e.g. link.).",
    schema: r#"{"type":"object","properties":{"since":{"type":"integer","minimum":0,"description":"first sequence number (the next= of the previous answer)"},"level":{"type":"string","enum":["error","warn","info","debug","trace"]},"subsystem":{"type":"string"},"name":{"type":"string","description":"prefix of the event name"},"max":{"type":"integer","minimum":1,"maximum":2000,"description":"lines, default 200"}},"additionalProperties":false}"#,
    handler: log,
);

/// Parses a JSON argument text and calls the tool as the MCP server would (tests).
pub fn call(args: &str) -> Result<String, ToolError> {
    call_tool("log", args)
}

/// `call` for any tool by name (`events`, RB-7a).
pub fn call_tool(name: &str, args: &str) -> Result<String, ToolError> {
    let t = mcp::find(name).ok_or_else(|| ToolError::Failed(format!("{name} tool not registered")))?;
    let args = Json::parse(args).map_err(|e| ToolError::Failed(format!("bad test JSON at {}", e.at)))?;
    let ctx = ToolContext { caller: policy::Subject::KERNEL, deadline_ns: time::now_ns().saturating_add(10_000_000_000) };
    match (t.handler)(&ctx, &args)?.into_iter().next() {
        Some(ToolOutput::Text(s)) => Ok(s),
        _ => Err(ToolError::Failed(String::from("no text answer"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use klog::{Gap, Record};

    fn rec(seq: u64, name: &'static str) -> Record {
        Record { seq, time_ns: 1_500_000_000, cpu: 0, thread: 1, level: Level::Warn, subsystem: "net", name, text: String::from("a=1") }
    }

    #[test]
    fn renders_head_records_and_gaps_in_order() {
        let page = Page {
            records: alloc::vec![rec(5, "x.one"), rec(9, "x.two")],
            gaps: alloc::vec![Gap { before: 5, count: 2 }, Gap { before: 9, count: 3 }, Gap { before: 12, count: 2 }],
            next: 12,
            oldest: 5,
            ahead: false,
            busy: false,
        };
        let text = render(&page);
        let lines: alloc::vec::Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "log next=12 oldest=5 shown=2 missed=7");
        assert_eq!(lines[1], "[... 2 records lost]");
        assert_eq!(lines[2], "#5 [    1.500000] W net x.one a=1");
        assert_eq!(lines[3], "[... 3 records lost]");
        assert!(lines[4].starts_with("#9 "));
        assert_eq!(lines[5], "[... 2 records lost]");
    }

    #[test]
    fn a_busy_ring_says_so() {
        let page = Page { next: 40, oldest: 3, busy: true, ..Page::default() };
        let head = render(&page);
        assert!(head.starts_with("log next=40 oldest=3 shown=0 missed=0 busy"), "{head}");
    }

    #[test]
    fn bad_arguments_name_themselves() {
        let ctx = ToolContext { caller: policy::Subject::KERNEL, deadline_ns: u64::MAX };
        let e = |s: &str| log(&ctx, &Json::parse(s).unwrap()).err();
        assert_eq!(e(r#"{"level":"loud"}"#), Some(bad("level")));
        assert_eq!(e(r#"{"since":"x"}"#), Some(bad("since")));
        assert_eq!(e(r#"{"since":-1}"#), Some(bad("since")));
        assert_eq!(e(r#"{"max":0}"#), Some(bad("max")));
        assert_eq!(e(r#"{"max":2001}"#), Some(bad("max")));
        assert_eq!(e(r#"{"subsystem":3}"#), Some(bad("subsystem")));
        assert_eq!(e(r#"[1]"#), Some(bad("arguments")));
        assert!(e(r#"{"since":0,"level":"warn","subsystem":"net","name":"link.","max":5}"#).is_none());
        assert!(e("null").is_none());
    }
}
