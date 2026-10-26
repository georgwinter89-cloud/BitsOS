//! `cpus`: the processors and what runs on them (`V2-PLAN.md` §4 "MCP tool `cpus` samples all CPUs"). Today:
//! the CPUs the firmware lists (MADT: APIC id, enabled, online-capable), how many run kernel code (one until
//! CP-4 starts the APs), and every kernel thread with its scheduling class (B4) and state. RB-3 adds per-CPU
//! samples (heartbeat, idle, NMI backtrace of a stalled CPU) to this tool once the watchdog exists.

use crate::{Json, ToolContext, ToolResult, mcp_tool};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

fn cpus(_ctx: &ToolContext, _args: &Json) -> ToolResult {
    let mut out = Vec::new();
    let madt = acpi::live::tables().and_then(|a| a.madt.as_ref());
    out.push(format!(
        "cpus online={} this={} firmware_listed={} firmware_enabled={}",
        hal::percpu::cpu_count(),
        hal::percpu::this_cpu(),
        madt.map(|m| format!("{}", m.cpus.len())).unwrap_or_else(|| String::from("unknown")),
        madt.map(|m| format!("{}", m.enabled_cpus())).unwrap_or_else(|| String::from("unknown")),
    ));
    if let Some(m) = madt {
        for (i, c) in m.cpus.iter().enumerate() {
            out.push(format!(
                "cpu {i} apic_id={} uid={} enabled={} online_capable={}{}",
                c.apic_id,
                c.uid,
                c.enabled,
                c.online_capable,
                if c.x2apic { " x2apic-entry" } else { "" }
            ));
        }
    }
    let mut th = sched::threads();
    th.sort_by_key(|t| t.0);
    out.push(format!("threads n={}", th.len()));
    for (id, name, class, state) in th {
        out.push(format!("thread {id} {name} class={class:?} state={state}"));
    }
    super::text(out)
}

mcp_tool!(
    name: "cpus",
    owner: "MC",
    safety: Read,
    description: "The processors: CPUs online in the kernel and the ones the firmware lists (APIC id, enabled), and every kernel thread with its scheduling class and state.",
    schema: r#"{"type":"object","properties":{}}"#,
    handler: cpus,
);
