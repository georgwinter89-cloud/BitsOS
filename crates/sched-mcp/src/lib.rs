//! MCP tools of lane SC (owner: SC). `ksync` and `sched` cannot register tools themselves: `mcp` depends on
//! `sched` (and so on `ksync`), so the registration lives in this crate on top of both.
//!
//! `locks` (Safety read-only): the hold/wait statistics per lock class and the lock-order checker's findings
//! (`ksync::stats::report`). Arguments: `name` (class name prefix), `sort` (wait, hold, acquired, contended,
//! name), `max` (lines, 1..10000, default 50), `all` (also classes never taken).
//!
//! `sched` (Safety read-only, SC-3): per-CPU scheduler state and, with `threads`, every thread (`sched_tool`).
//!
//! `sched_focus` (Safety change, SC-7a): the program in front and the compositor/input grants (`focus_tool`).
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod focus_tool;
mod sched_tool;
mod user_fpu;

use alloc::string::String;
use ksync::stats::{ReportOpts, SortBy};
use mcp::{Json, ToolContext, ToolError, ToolOutput, ToolResult};

const DEFAULT_MAX: u64 = 50;
/// Bound on one answer's size (a request limit, not a count of things).
const MAX_LINES: u64 = 10_000;

fn opts(args: &Json) -> Result<ReportOpts, ToolError> {
    let bad = |n: &str| ToolError::BadArgument(String::from(n));
    let mut o = ReportOpts { max: DEFAULT_MAX as usize, ..Default::default() };
    if let Some(v) = args.get("name").filter(|v| !v.is_null()) {
        o.prefix = Some(String::from(v.as_str().ok_or_else(|| bad("name"))?));
    }
    if let Some(v) = args.get("sort").filter(|v| !v.is_null()) {
        o.sort = v.as_str().and_then(SortBy::parse).ok_or_else(|| bad("sort"))?;
    }
    if let Some(v) = args.get("max").filter(|v| !v.is_null()) {
        o.max = v.as_u64().filter(|m| (1..=MAX_LINES).contains(m)).ok_or_else(|| bad("max"))? as usize;
    }
    if let Some(v) = args.get("all").filter(|v| !v.is_null()) {
        o.all = v.as_bool().ok_or_else(|| bad("all"))?;
    }
    Ok(o)
}

fn locks(_ctx: &ToolContext, args: &Json) -> ToolResult {
    let o = opts(args)?;
    Ok(alloc::vec![ToolOutput::Text(ksync::stats::report(&o))])
}

mcp::mcp_tool!(
    name: "locks",
    owner: "SC",
    safety: Read,
    description: "Kernel locks: per lock class (= lock name) how often taken and contended, total and longest wait, average and longest hold, failed tries, timeouts; then the lock-order checker's violations (debug kernels). Sorted by total wait unless `sort` says otherwise.",
    schema: r#"{"type":"object","properties":{"name":{"type":"string","description":"only classes whose name starts with this"},"sort":{"type":"string","enum":["wait","hold","acquired","contended","name"]},"max":{"type":"integer","minimum":1,"maximum":10000,"description":"at most this many classes (default 50)"},"all":{"type":"boolean","description":"also classes never taken"}}}"#,
    handler: locks,
);

fn ktest_locks_tool() -> ktest::TestResult {
    let t = mcp::find("locks").ok_or("tool locks not registered")?;
    ktest::kassert!(t.owner == "SC" && t.safety == mcp::Safety::Read);
    let ctx = ToolContext { caller: policy::Subject::KERNEL, deadline_ns: u64::MAX };
    let args = Json::parse(r#"{"name":"sched","sort":"name","max":5}"#).map_err(|e| alloc::format!("{e:?}"))?;
    let out = (t.handler)(&ctx, &args).map_err(|e| alloc::format!("{e:?}"))?;
    let text = match out.first() {
        Some(ToolOutput::Text(s)) => s.clone(),
        _ => return Err(String::from("no text answer")),
    };
    ktest::kassert!(text.starts_with("locks classes="), "{text}");
    ktest::kassert!(text.contains("\nsched sched:770 "), "{text}");
    let bad = Json::parse(r#"{"sort":"loudest"}"#).map_err(|e| alloc::format!("{e:?}"))?;
    ktest::kassert!(matches!((t.handler)(&ctx, &bad), Err(ToolError::BadArgument(ref n)) if n == "sort"));
    Ok(())
}
ktest::ktest!("sc.locks.mcp_tool", Safe, ktest_locks_tool);

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    fn run(a: &str) -> Result<String, ToolError> {
        let ctx = ToolContext { caller: policy::Subject::KERNEL, deadline_ns: u64::MAX };
        let out = locks(&ctx, &Json::parse(a).unwrap())?;
        match out.into_iter().next() {
            Some(ToolOutput::Text(s)) => Ok(s),
            _ => panic!("no text"),
        }
    }

    #[test]
    fn arguments_and_answer() {
        let l = ksync::SpinLock::named("t.mcp.locks", 0);
        for _ in 0..3 {
            *l.lock() += 1;
        }
        let t = run(r#"{"name":"t.mcp."}"#).unwrap();
        assert!(t.starts_with("locks classes=1 shown=1 "), "{t}");
        assert!(t.contains("\nt.mcp.locks subsystem:500 3 0 "), "{t}");
        let all = run(r#"{"all":true,"sort":"name","max":10000}"#).unwrap();
        assert!(all.contains("\nheap alloc:900 ") && all.contains("\npci.enable pci:200 "), "{all}");
        let one = run(r#"{"all":true,"max":1}"#).unwrap();
        assert_eq!(one.lines().count(), 3, "{one}");
        assert_eq!(run(r#"{"max":0}"#), Err(ToolError::BadArgument("max".into())));
        assert_eq!(run(r#"{"sort":"x"}"#), Err(ToolError::BadArgument("sort".into())));
        assert_eq!(run(r#"{"name":3}"#), Err(ToolError::BadArgument("name".into())));
        assert_eq!(run(r#"{"all":"yes"}"#), Err(ToolError::BadArgument("all".into())));
        assert!(run(r#"{"name":null}"#).is_ok());
    }

    #[test]
    fn registered_with_owner_and_safety() {
        let t = mcp::find("locks").unwrap();
        assert_eq!(t.owner, "SC");
        assert_eq!(t.safety, mcp::Safety::Read);
        assert!(Json::parse(t.input_schema).is_ok());
    }
}
