//! MCP tool `latency` (owner IN, Read; IN-6, B4): input-to-screen latency per frame, from the input report's
//! arrival (the evdev timestamp = the controller interrupt's time, usb-hid's `on_report_at`, US-ev) to the
//! frame that shows the reaction leaving for the framebuffer copy, by path (console echo, pointer, pointer over a fullscreen program, a program's own frame).
//! The numbers live in `console::latency`; this crate registers the tool because the console cannot depend on
//! mcp (mcp depends on the console).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use console::latency::{self, PathStats, Sample};
use mcp::{Json, ToolContext, ToolError, ToolOutput, ToolResult};

/// Most recent frames one answer lists (a bound on one request).
const LAST_MAX: u64 = 4096;

fn arg_u64(args: &Json, key: &str) -> Result<Option<u64>, ToolError> {
    match args.get(key) {
        None => Ok(None),
        Some(v) => v.as_u64().map(Some).ok_or_else(|| ToolError::BadArgument(format!("{key}: want a non-negative integer"))),
    }
}

/// Microseconds with one decimal.
fn us(ns: u64) -> String {
    format!("{}.{}", ns / 1000, ns % 1000 / 100)
}

fn path_line(name: &str, p: &PathStats) -> String {
    if p.frames == 0 {
        return format!("{name}: frames=0 stale={}", p.stale);
    }
    let n = p.frames;
    format!(
        "{name}: frames={n} min_us={} avg_us={} p50_us<={} p99_us<={} max_us={} over_20ms={} stale={} | queue_avg_us={} draw_avg_us={} copy_avg_us={}",
        us(p.min_ns),
        us(p.sum_ns / n),
        us(p.percentile_ns(50)),
        us(p.percentile_ns(99)),
        us(p.max_ns),
        p.over_budget,
        p.stale,
        us(p.queue_sum_ns / n),
        us(p.draw_sum_ns / n),
        us(p.copy_sum_ns / n)
    )
}

fn frame_line(s: &Sample) -> String {
    format!(
        "{} arrival_ns={} total_us={} queue_us={} draw_us={} copy_us={}",
        s.path.name(),
        s.arrival_ns,
        us(s.total_ns()),
        us(s.picked_ns.saturating_sub(s.arrival_ns)),
        us(s.start_ns.saturating_sub(s.picked_ns)),
        us(s.done_ns.saturating_sub(s.start_ns))
    )
}

fn latency_tool(_ctx: &ToolContext, args: &Json) -> ToolResult {
    let last = arg_u64(args, "last")?.unwrap_or(16);
    if last > LAST_MAX {
        return Err(ToolError::BadArgument(format!("last: at most {LAST_MAX}")));
    }
    let keep = arg_u64(args, "keep")?;
    if keep.is_some_and(|k| k == 0 || k > LAST_MAX) {
        return Err(ToolError::BadArgument(format!("keep: 1..={LAST_MAX}")));
    }
    let reset = match args.get("reset") {
        None => false,
        Some(v) => v.as_bool().ok_or_else(|| ToolError::BadArgument(String::from("reset: want true or false")))?,
    };
    let (paths, recent) = latency::report();
    let mut out: Vec<String> = Vec::new();
    out.push(String::from("input-to-screen latency per frame (report arrival = the USB interrupt -> the frame leaves for the framebuffer copy; stages: queue = interrupt to the input thread, draw = to the copy, copy = the copy itself)"));
    for (p, st) in &paths {
        out.push(path_line(p.name(), st));
    }
    out.push(format!("pointer over a program: drawn_at={:?} keep={}", console::pointer_over::drawn_at(), latency::keep()));
    let shown = (last as usize).min(recent.len());
    out.push(format!("recent frames n={} shown={} (newest last)", recent.len(), shown));
    for s in &recent[recent.len() - shown..] {
        out.push(frame_line(s));
    }
    if let Some(k) = keep {
        latency::set_keep(k as usize);
        out.push(format!("keep set to {k}"));
    }
    if reset {
        latency::reset();
        out.push(String::from("numbers reset (after this answer)"));
    }
    Ok(alloc::vec![ToolOutput::Text(out.join("\n"))])
}

mcp::mcp_tool!(
    name: "latency",
    owner: "IN",
    safety: Read,
    description: "Input-to-screen latency per frame (B4): from a USB input report's arrival to the frame showing its reaction leaving for the framebuffer copy, per path (console echo, pointer, pointer over a fullscreen program, a program's own frame), with min/avg/p50/p99/max in microseconds, frames over the 20 ms budget and the recent frames with their stages. last: how many recent frames to list (default 16); keep: how many recent frames to keep from now on; reset: true forgets the numbers after answering (start a measurement).",
    schema: r#"{"type":"object","properties":{"last":{"type":"integer","minimum":0,"maximum":4096},"keep":{"type":"integer","minimum":1,"maximum":4096},"reset":{"type":"boolean"}}}"#,
    handler: latency_tool,
);

#[cfg(test)]
mod tests {
    use super::*;
    use console::latency::{Path, Sample};
    use mcp::ToolError;

    fn call(args: &str) -> Result<String, ToolError> {
        let args = Json::parse(args).expect("test args parse");
        let ctx = ToolContext::detached(u64::MAX);
        latency_tool(&ctx, &args).map(|out| match &out[..] {
            [ToolOutput::Text(t)] => t.clone(),
            other => panic!("want one text output, got {other:?}"),
        })
    }

    fn bad(args: &str) {
        match call(args) {
            Err(ToolError::BadArgument(_)) => {}
            other => panic!("{args}: want BadArgument, got {other:?}"),
        }
    }

    /// One test: the numbers are global, so keep, reset and the hostile set (MC change request IN-6/2) run in
    /// order.
    #[test]
    fn arguments_output_keep_and_reset() {
        for a in [
            r#"{"last":-1}"#,
            r#"{"last":"x"}"#,
            r#"{"last":5000}"#,
            r#"{"last":1.5}"#,
            r#"{"keep":0}"#,
            r#"{"keep":5000}"#,
            r#"{"keep":1.5}"#,
            r#"{"reset":"yes"}"#,
            r#"{"reset":1}"#,
        ] {
            bad(a);
        }
        let t = call(r#"{"reset":true}"#).unwrap();
        assert!(t.contains("numbers reset"), "{t}");
        for p in latency::PATHS {
            assert!(t.contains(&format!("\n{}: frames=", p.name())), "{} missing:\n{t}", p.name());
        }
        let s = |i: u64| Sample { path: Path::Pointer, arrival_ns: 1_000 * i, picked_ns: 1_000 * i + 100, start_ns: 1_000 * i + 2_000, done_ns: 1_000 * i + 3_000 };
        call(r#"{"keep":3}"#).unwrap();
        for i in 1..=5 {
            latency::record(s(i));
        }
        let t = call(r#"{"last":2}"#).unwrap();
        assert!(t.contains("pointer: frames=5 "), "{t}");
        assert!(t.contains("recent frames n=3 shown=2"), "{t}");
        assert!(t.contains("arrival_ns=5000 ") && t.contains("arrival_ns=4000 ") && !t.contains("arrival_ns=3000 "), "{t}");
        assert!(t.contains("keep=3"), "{t}");
        let t = call(r#"{"reset":true,"keep":64}"#).unwrap();
        assert!(t.contains("pointer: frames=5 "), "the answer shows the numbers before the reset:\n{t}");
        let t = call("{}").unwrap();
        assert!(t.contains("pointer: frames=0 ") && t.contains("recent frames n=0 shown=0"), "{t}");
    }
}
