//! The `sched_focus` MCP tool (owner SC, Safety change; SC-7a, B4): makes a process the program in front, or
//! grants/revokes the compositor or input grant, so its threads run in the Interactive class. It stands in for
//! the compositor (bitscomp, WY) until that calls `sched::interactive::set_focus` itself. Without `pid` it only
//! reports. A pid is a thread group (tgid); 0 with the focus grant clears the focus.

use alloc::format;
use alloc::string::String;
use mcp::{Json, ToolContext, ToolError, ToolOutput, ToolResult};
use sched::interactive::{self, Grant, GrantError};

/// The report (pure, host-tested).
pub fn render(focus: Option<u64>, grants: &[(u64, u8)], did: Option<&str>) -> String {
    let mut s = String::new();
    if let Some(d) = did {
        s += d;
        s += "\n";
    }
    s += &format!("focus={}\n", focus.map_or(String::from("none"), |f| format!("{f}")));
    s += "group grants\n";
    for (g, b) in grants {
        s += &format!("{g} {}\n", interactive::bits_names(*b));
    }
    s
}

fn tool(_ctx: &ToolContext, args: &Json) -> ToolResult {
    let bad = |n: &str| ToolError::BadArgument(String::from(n));
    let grant = match args.get("grant").filter(|v| !v.is_null()) {
        Some(v) => Grant::from_name(v.as_str().ok_or_else(|| bad("grant"))?).ok_or_else(|| bad("grant"))?,
        None => Grant::Focus,
    };
    let revoke = match args.get("revoke").filter(|v| !v.is_null()) {
        Some(v) => v.as_bool().ok_or_else(|| bad("revoke"))?,
        None => false,
    };
    let did = match args.get("pid").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => {
            let pid = v.as_u64().ok_or_else(|| bad("pid"))?;
            let r = if grant == Grant::Focus && !revoke {
                interactive::set_focus(pid).map(|before| format!("focus {pid} (before: {before}), threads {}", interactive::group_threads(pid).len()))
            } else {
                interactive::set_grant(pid, grant, !revoke).map(|n| format!("{} {} {pid}, threads {n}", if revoke { "revoked" } else { "granted" }, grant.name()))
            };
            Some(r.map_err(|e| match e {
                GrantError::NoGroup => ToolError::Failed(format!("no process {pid} with live threads")),
                GrantError::NoMemory => ToolError::Failed(String::from("no memory for the grant list")),
            })?)
        }
    };
    Ok(alloc::vec![ToolOutput::Text(render(interactive::focus(), &interactive::grants(), did.as_deref()))])
}

mcp::mcp_tool!(
    name: "sched_focus",
    owner: "SC",
    safety: Change,
    description: "B4: run a process's threads in the Interactive scheduling class (stands in for the compositor until bitscomp sets the program in front). pid: the process (tgid); grant: focus (default; the one program in front, the one before loses it; pid 0 clears it), compositor or input; revoke=true takes the grant away. Without pid: only the report (group in front, granted groups). Grants end when the process's last thread exits.",
    schema: r#"{"type":"object","properties":{"pid":{"type":"integer","minimum":0,"description":"process id (tgid); 0 with grant focus clears the focus"},"grant":{"type":"string","enum":["focus","compositor","input"],"description":"which grant (default focus)"},"revoke":{"type":"boolean","description":"take the grant away"}}}"#,
    handler: tool,
);

fn ktest_focus_tool() -> ktest::TestResult {
    use core::sync::atomic::{AtomicBool, Ordering};
    let t = mcp::find("sched_focus").ok_or("tool sched_focus not registered")?;
    ktest::kassert!(t.owner == "SC" && t.safety == mcp::Safety::Change);
    let ctx = ToolContext { caller: policy::Subject::KERNEL, deadline_ns: u64::MAX };
    let call = |a: &str| -> Result<String, ToolError> {
        let args = Json::parse(a).map_err(|e| ToolError::Failed(format!("{e:?}")))?;
        match (t.handler)(&ctx, &args)?.first() {
            Some(ToolOutput::Text(s)) => Ok(s.clone()),
            _ => Err(ToolError::Failed(String::from("no text answer"))),
        }
    };
    let prev = interactive::focus().unwrap_or(0);
    // A thread of its own group far above any pid, so the test never touches a real program.
    static NEXT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new((1 << 48) + (1 << 40));
    let g = NEXT.fetch_add(1, Ordering::Relaxed);
    let stop = alloc::sync::Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let h = sched::spawn_in_group("sc7-mcp", sched::Class::Normal, g, move || {
        while !s.load(Ordering::Acquire) {
            sched::sleep_ms(1);
        }
    })
    .map_err(|e| format!("spawn: {e:?}"))?;
    let r = (|| -> ktest::TestResult {
        let out = call(&format!(r#"{{"pid":{g}}}"#)).map_err(|e| format!("{e:?}"))?;
        ktest::kassert!(out.contains(&format!("focus={g}\n")) && h.thread().class() == sched::Class::Interactive, "{out}");
        let out = call(&format!(r#"{{"pid":{g},"grant":"input"}}"#)).map_err(|e| format!("{e:?}"))?;
        ktest::kassert!(out.contains(&format!("\n{g} input,focus\n")), "{out}");
        call(&format!(r#"{{"pid":{g},"grant":"input","revoke":true}}"#)).map_err(|e| format!("{e:?}"))?;
        let out = call(r#"{"pid":0}"#).map_err(|e| format!("{e:?}"))?;
        ktest::kassert!(out.contains("focus=none\n") && h.thread().class() == sched::Class::Normal, "{out}");
        ktest::kassert!(matches!(call(r#"{"grant":"x"}"#), Err(ToolError::BadArgument(ref n)) if n == "grant"));
        ktest::kassert!(matches!(call(r#"{"pid":-1}"#), Err(ToolError::BadArgument(ref n)) if n == "pid"));
        ktest::kassert!(matches!(call(&format!(r#"{{"pid":{}}}"#, g + (1 << 20))), Err(ToolError::Failed(_))), "a group without threads");
        let out = call("{}").map_err(|e| format!("{e:?}"))?;
        ktest::kassert!(out.starts_with("focus=none\ngroup grants\n"), "{out}");
        Ok(())
    })();
    stop.store(true, Ordering::Release);
    if interactive::set_focus(prev).is_err() {
        let _ = interactive::set_focus(0);
    }
    ktest::kassert!(h.join_timeout(30_000_000_000), "the test thread did not end");
    r
}
ktest::ktest!("sched.interactive.mcp_focus", Safe, ktest_focus_tool);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_the_report() {
        assert_eq!(render(None, &[], None), "focus=none\ngroup grants\n");
        assert_eq!(render(Some(9), &[(9, 4), (12, 3)], Some("focus 9 (before: 0), threads 2")), "focus 9 (before: 0), threads 2\nfocus=9\ngroup grants\n9 focus\n12 compositor,input\n");
    }
}
