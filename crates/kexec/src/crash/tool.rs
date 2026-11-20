//! MCP tools of the panic path (owner RB): `crash_info` (read: the crash page, the boot marker, the disk copy
//! (CRASHDISK), the newest record the boots before left with its source, the symbol table, what a panic would do;
//! `{"list":true}` lists every record found, `{"index":N}` shows one) and `crash_test` (change: a deliberate kernel
//! panic after the answer, for the Mini-PC check of the crash record across a reset, in RAM and on the boot ESP).

use super::*;
use alloc::boxed::Box;
use alloc::format;
use alloc::vec;
use mcp::{Json, ToolContext, ToolError, ToolOutput, ToolResult, mcp_tool};

fn crash_info(_ctx: &ToolContext, args: &Json) -> ToolResult {
    Ok(vec![ToolOutput::Text(info_text(args))])
}

/// The disk copy's state line: as decided at boot, unless the panic writer refuses now (the boot window closed or
/// changed since arming, e.g. /boot unmounted or a rescan lost it).
fn disk_line() -> String {
    line_of(disk::state(), disk::armed().map(|a| a.dev.ready()))
}

pub(crate) fn line_of(st: disk::State, ready: Option<Result<(), &str>>) -> String {
    match st {
        disk::State::Waiting => String::from("disk copy: waiting for /boot (the record is in RAM only until then)\n"),
        disk::State::Off(why) => format!("disk copy: off, RAM only: {why}\n"),
        disk::State::Failed(why) => format!("disk copy: failed, RAM only: {why}\n"),
        disk::State::Armed { disk, .. } if matches!(ready, Some(Err(_)) | None) => {
            let why = ready.and_then(|r| r.err()).unwrap_or("no panic writer");
            format!("disk copy: armed on {disk} at boot, now RAM only: {why}\n")
        }
        disk::State::Armed { disk, slot, seq, runs, first_lba } => {
            format!("disk copy: armed on {disk}, \\BITSOS2\\CRASH\\SLOTS.BIN, the next panic writes slot {slot} as number {seq} ({runs} run(s), first LBA {first_lba})\n")
        }
    }
}

/// `crash_info`'s text (also the ktests'): with `list` every record found, with `index` that one.
pub(crate) fn info_text(args: &Json) -> String {
    let all = disk::show::records();
    if args.get("list").and_then(|v| v.as_bool()) == Some(true) {
        let mut t = format!("{} record(s), newest first:\n", all.len());
        for (i, r) in all.iter().enumerate() {
            t.push_str(&disk::show::summary(i, r));
            t.push('\n');
        }
        return t;
    }
    if let Some(i) = args.get("index").and_then(|v| v.as_u64()) {
        return match all.get(i as usize) {
            Some(r) => format!("{}\n{}", disk::show::summary(i as usize, r), r.text),
            None => format!("no record {i} ({} found)\n", all.len()),
        };
    }
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
    t.push_str(&disk_line());
    match all.first() {
        Some(r) => {
            t.push_str(&format!("previous crash, source: {}\n{}\n", disk::show::summary(0, r), r.text));
            if all.len() > 1 {
                t.push_str(&format!("({} older record(s): crash_info {{\"list\":true}})\n", all.len() - 1));
            }
        }
        None => t.push_str("previous crash: none\n"),
    }
    t
}

mcp_tool!(
    name: "crash_info",
    owner: "RB",
    safety: Read,
    description: "The panic path's state: what a panic does now (CONFIG panic=halt|reboot, panic_delay=), the crash page and whether the boot marker survived the last reset (kept = RAM kept its contents), the disk copy on the boot ESP (armed, or why the record stays in RAM only), the symbol table for backtraces, and the newest crash record with its source (RAM or disk: message, backtraces, registers of every CPU, the newest log lines). list=true lists every record found (RAM and the disk's slots, newest first); index=N shows record N.",
    schema: r#"{"type":"object","properties":{"list":{"type":"boolean","description":"list every record found, newest first"},"index":{"type":"integer","description":"show record N of the list"}}}"#,
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
    description: "Panics the kernel on purpose after answering (confirm=\"panic\" required): kind=panic (a Rust panic, default) or kind=fault (a kernel #GP). The panic path stops the other CPUs by NMI, logs backtraces and registers, writes the crash record (in RAM, and on the boot ESP when crash_info says the disk copy is armed) and then halts or resets per CONFIG panic= (hardware default: reset after 30 s). For checking that the record survives a reset, also a cold one (the disk copy).",
    schema: r#"{"type":"object","properties":{"confirm":{"type":"string","description":"must be \"panic\""},"kind":{"type":"string","enum":["panic","fault"]}},"required":["confirm"]}"#,
    handler: crash_test,
);
