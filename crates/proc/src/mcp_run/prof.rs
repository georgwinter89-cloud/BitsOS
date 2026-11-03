//! `prof` (FD-1 measuring build only, `--cfg bitsos_fd_measure`): samples where one process's user code runs.
//! For `secs` seconds every `us` microseconds each busy CPU gets an interrupt (`sched::prof_kick`); those that
//! interrupt user code of the process record its rip and FS base (`hal::prof`). The answer: samples per thread
//! (FS base), the hottest 4 KiB pages and the hottest rips (map them with /proc/<pid>/maps). Never in a
//! release build.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use mcp::{Json, ToolContext, ToolError, ToolOutput, ToolResult};

fn top<K: Copy + Ord>(m: &BTreeMap<K, u64>, n: usize) -> Vec<(K, u64)> {
    let mut v: Vec<(K, u64)> = m.iter().map(|(k, c)| (*k, *c)).collect();
    v.sort_by_key(|(_, c)| core::cmp::Reverse(*c));
    v.truncate(n);
    v
}

fn tool(_ctx: &ToolContext, args: &Json) -> ToolResult {
    let bad = |n: &str| ToolError::BadArgument(String::from(n));
    let pid = args.get("pid").and_then(|v| v.as_u64()).ok_or_else(|| bad("pid"))?;
    let us = args.get("us").and_then(|v| v.as_u64()).unwrap_or(250).clamp(50, 100_000);
    let secs = args.get("secs").and_then(|v| v.as_u64()).unwrap_or(10).clamp(1, 60);
    let p = crate::find(pid as u32).ok_or_else(|| ToolError::Failed(format!("no process {pid}")))?;
    hal::prof::target(p.mm.root());
    let end = time::now_ns() + secs * 1_000_000_000;
    let mut kicks = 0u64;
    while time::now_ns() < end {
        sched::prof_kick();
        kicks += 1;
        sched::sleep_ns(us * 1000);
    }
    hal::prof::target(0);
    let s = hal::prof::samples();
    let (mut by_thread, mut by_page, mut by_rip) = (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
    for &(rip, fs) in &s {
        *by_thread.entry(fs).or_insert(0u64) += 1;
        *by_page.entry(rip & !0xFFF).or_insert(0u64) += 1;
        *by_rip.entry(rip).or_insert(0u64) += 1;
    }
    let mut out = format!("prof pid={pid} secs={secs} us={us} kicks={kicks} samples={}\n", s.len());
    let mut sc = hal::prof::syscalls();
    sc.sort_by_key(|(_, c)| core::cmp::Reverse(*c));
    out += "syscalls (nr count)";
    for (nr, c) in sc.iter().take(25) {
        let _ = write!(out, " {nr}:{c}");
    }
    out += "\nthreads (fs_base samples; 0xffffffffffffffff = kernel code)\n";
    for (fs, c) in top(&by_thread, 20) {
        let _ = writeln!(out, "{fs:#x} {c}");
    }
    out += "pages (page samples)\n";
    for (pg, c) in top(&by_page, 40) {
        let _ = writeln!(out, "{pg:#x} {c}");
    }
    out += "rips (rip samples)\n";
    for (r, c) in top(&by_rip, 80) {
        let _ = writeln!(out, "{r:#x} {c}");
    }
    // `lo`/`hi` (hex text): for samples with a rip in that range, where rsi and rdi pointed (1 MiB granules).
    let hexarg = |k: &str| args.get(k).and_then(|v| v.as_str()).and_then(|t| u64::from_str_radix(t.trim_start_matches("0x"), 16).ok());
    if let (Some(lo), Some(hi)) = (hexarg("lo"), hexarg("hi")) {
        let ops = hal::prof::operands(lo, hi);
        let (mut src, mut dst) = (BTreeMap::new(), BTreeMap::new());
        for &(si, di) in &ops {
            *src.entry(si >> 20).or_insert(0u64) += 1;
            *dst.entry(di >> 20).or_insert(0u64) += 1;
        }
        let _ = writeln!(out, "operands in [{lo:#x}, {hi:#x}): {} samples\nrsi (MiB granule samples)", ops.len());
        for (g, c) in top(&src, 30) {
            let _ = writeln!(out, "{:#x} {c}", g << 20);
        }
        out += "rdi (MiB granule samples)\n";
        for (g, c) in top(&dst, 30) {
            let _ = writeln!(out, "{:#x} {c}", g << 20);
        }
    }
    Ok(alloc::vec![ToolOutput::Text(out)])
}

mcp::mcp_tool!(
    name: "prof",
    owner: "SC",
    safety: Read,
    description: "FD-1 measuring build: samples one process's user program counter for secs seconds (interrupts to busy CPUs every us microseconds); answers samples per thread (FS base), the hottest 4 KiB pages and rips.",
    schema: r#"{"type":"object","properties":{"pid":{"type":"integer","minimum":1},"us":{"type":"integer","minimum":50,"maximum":100000},"secs":{"type":"integer","minimum":1,"maximum":60},"lo":{"type":"string"},"hi":{"type":"string"}},"required":["pid"]}"#,
    handler: tool,
);
