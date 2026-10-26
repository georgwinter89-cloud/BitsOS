//! MCP tools of the panic path (owner RB): `crash_info` (read: the crash page, the boot marker, the record the
//! boot before left, the symbol table, what a panic would do) and `crash_test` (change: a deliberate kernel
//! panic after the answer, for the Mini-PC check of the crash record across a warm reset).

use super::*;
use alloc::boxed::Box;
use alloc::format;
use alloc::vec;
use mcp::{Json, ToolContext, ToolError, ToolOutput, ToolResult, mcp_tool};

fn crash_info(_ctx: &ToolContext, _args: &Json) -> ToolResult {
    let mut t = String::new();
    let a = match action() {
        Action::Halt { qemu_exit: true } => String::from("halt (QEMU: panic exit code)"),
        Action::Halt { .. } => String::from("halt"),
        Action::Reboot { secs } => format!("reboot after {secs} s"),
    };
    t.push_str(&format!("after a panic: {a} (CONFIG panic={} panic_delay={})\n", mode_name(MODE.load(Ordering::Acquire)), DELAY_S.load(Ordering::Acquire)));
    match hal::panic::crash_page() {
        Some((p, l)) => t.push_str(&format!("crash page {p:#x} ({l} bytes)\n")),
        None => t.push_str("crash page: none (crashes are logged, not kept)\n"),
    }
    match marker() {
        Some((f, m)) => {
            let m = m.unwrap_or_default();
            t.push_str(&format!(
                "boot marker at start: {} (previous boot {} generation {} reload {} uptime {} s)\n",
                f.name(),
                hex16(&m.boot_id).as_str(),
                m.generation,
                m.reload,
                m.time_ns / 1_000_000_000
            ));
        }
        None => t.push_str("boot marker: not read\n"),
    }
    match symbols() {
        Some((s, src)) => t.push_str(&format!("symbol table: {} functions from {src} ({} KiB)\n", s.len(), s.bytes() / 1024)),
        None => t.push_str("symbol table: none (backtraces show addresses only)\n"),
    }
    t.push_str(&format!("NMI slots: {}\n", if hal::nmi::prepared() { "ready" } else { "not prepared" }));
    match previous() {
        Some(p) => {
            let h = &p.header;
            t.push_str(&format!(
                "previous crash (boot {}, generation {}, reload {}, after {}.{:03} s on CPU {}, {} of {} other CPUs answered{}{}{}):\n{}",
                hex16(&h.boot_id).as_str(),
                h.generation,
                h.reload,
                h.time_ns / 1_000_000_000,
                (h.time_ns / 1_000_000) % 1000,
                h.cpu,
                h.cpus_answered,
                h.cpus_asked,
                if h.flags & flags::EXCEPTION != 0 { ", CPU exception" } else { "" },
                if h.flags & flags::CUT != 0 { ", cut" } else { "" },
                if p.reported_before { ", logged by an earlier boot" } else { "" },
                p.text
            ));
        }
        None => t.push_str("previous crash: none\n"),
    }
    Ok(vec![ToolOutput::Text(t)])
}

mcp_tool!(
    name: "crash_info",
    owner: "RB",
    safety: Read,
    description: "The panic path's state: what a panic does now (CONFIG panic=halt|reboot, panic_delay=), the crash page and whether the boot marker survived the last reset (kept = RAM kept its contents), the symbol table for backtraces, and the crash record the boot before left (message, backtraces, registers of every CPU, the newest log lines).",
    schema: r#"{"type":"object","properties":{}}"#,
    handler: crash_info,
);

fn crash_test(ctx: &ToolContext, args: &Json) -> ToolResult {
    if args.get("confirm").and_then(|v| v.as_str()) != Some("panic") {
        return Err(ToolError::BadArgument(String::from("crash_test panics the kernel on purpose: pass confirm=\"panic\"")));
    }
    let kind = match args.get("kind").and_then(|v| v.as_str()) {
        None | Some("panic") => 0,
        Some("fault") => 1,
        Some(_) => return Err(ToolError::BadArgument(String::from("kind: panic or fault"))),
    };
    let a = action();
    klog::event!(Warn, "crash", "test.requested", kind = kind; "a deliberate kernel panic follows the answer");
    if !ctx.after_answer(Box::new(move || {
        ktests::force(kind);
    })) {
        return Err(ToolError::Failed(String::from("crash_test panics only after its answer, and this call has no answer to wait for")));
    }
    let then = match a {
        Action::Halt { .. } => String::from("the machine then halts (panic=halt): power it off and on"),
        Action::Reboot { secs } => format!("the machine resets after {secs} s and the next boot logs `crash previous`; `crash_info` shows the record"),
    };
    Ok(vec![ToolOutput::Text(format!("panicking once this answer is out; {then}"))])
}

mcp_tool!(
    name: "crash_test",
    owner: "RB",
    safety: Change,
    description: "Panics the kernel on purpose after answering (confirm=\"panic\" required): kind=panic (a Rust panic, default) or kind=fault (a kernel #GP). The panic path stops the other CPUs by NMI, logs backtraces and registers, writes the crash record and then halts or resets per CONFIG panic= (hardware default: reset after 30 s). For checking that the record survives a warm reset.",
    schema: r#"{"type":"object","properties":{"confirm":{"type":"string","description":"must be \"panic\""},"kind":{"type":"string","enum":["panic","fault"]}},"required":["confirm"]}"#,
    handler: crash_test,
);
