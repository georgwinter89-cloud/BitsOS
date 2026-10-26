//! MCP tools `watchdog` (Read: state, config, what the boot found, per-CPU liveness; `backtrace=true` adds an NMI
//! sample of every CPU) and `hang` (Change, `confirm="hang"`: the deliberate hang for Georg's A1 test).

use crate::system::{self, State};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use mcp::{Json, ToolContext, ToolError, ToolOutput, ToolResult, mcp_tool};

const MS: u64 = 1_000_000;

pub fn lines(backtrace: bool) -> Vec<String> {
    let mut out = Vec::new();
    let st = system::state();
    out.push(format!("watchdog state={} chip={}{}", st.name(), system::found().unwrap_or("(not looked yet)"), if system::on_fake() { " fake=yes" } else { "" }));
    match system::config() {
        Some(c) => out.push(format!(
            "config on={} ({}) timeout_s={} alive_s={} grace_s={} reload={}",
            c.on,
            if c.on_from_config { "CONFIG" } else { "default" },
            c.timeout_s,
            c.alive_s,
            c.grace_s,
            if c.reload_grace { "grace" } else { "stop" }
        )),
        None => out.push(String::from("config not read yet")),
    }
    if let Some(b) = system::boot_status() {
        out.push(format!("boot control={:#x} count={} fired={} was_running={}", b.ctl, b.count, b.fired, b.was_running));
    }
    if let Some(d) = system::system() {
        let now = d.now();
        let r = d.regs();
        out.push(format!(
            "kicks={} withheld={} checks={} probes={} last_kick_ms_ago={} armed={} chip_control={:#x} chip_count={}",
            d.kicks(),
            d.withheld(),
            d.checks(),
            d.probes(),
            now.saturating_sub(d.last_kick_ns()) / MS,
            d.armed(),
            r.ctl_read(),
            r.count_read()
        ));
        for v in system::views() {
            out.push(format!(
                "cpu {} live={} heartbeat={} quiet_ms={}{}{}",
                v.cpu,
                v.live.name(),
                v.heartbeat,
                v.age_ns / MS,
                v.probe_pending_ns.map(|p| format!(" probe_unanswered_ms={}", p / MS)).unwrap_or_default(),
                if v.reported { " reported" } else { "" }
            ));
        }
    } else if st == State::Armed {
        out.push(String::from("armed but no system dog (should not happen)"));
    }
    if backtrace {
        out.extend(system::backtraces(&[], "tool"));
    }
    out
}

fn watchdog(_ctx: &ToolContext, args: &Json) -> ToolResult {
    let bt = match args.get("backtrace") {
        None | Some(Json::Null) => false,
        Some(Json::Bool(b)) => *b,
        Some(_) => return Err(ToolError::BadArgument(String::from("backtrace must be true or false"))),
    };
    Ok(vec![ToolOutput::Text(lines(bt).join("\n"))])
}

mcp_tool!(
    name: "watchdog",
    owner: "RB",
    safety: Read,
    description: "The FCH watchdog: state (armed/off/absent/...), CONFIG (watchdog=, watchdog_timeout=, watchdog_alive=, watchdog_grace=, watchdog_reload=), what the boot found (FIRED = the last start ended by the watchdog), kicks and withheld kicks, and per CPU whether it is alive (heartbeat moved, idle with interrupts on, stalled). backtrace=true: NMI-samples every CPU and adds registers and backtraces.",
    schema: r#"{"type":"object","properties":{"backtrace":{"type":"boolean","description":"NMI-sample every CPU and show its backtrace"}}}"#,
    handler: watchdog,
);

/// Why `hang` refuses in this state (None: armed, it may hang).
pub fn hang_refusal(st: State) -> Option<String> {
    (st != State::Armed).then(|| format!("the watchdog is {} (not armed): a hang would need Georg to switch the machine off and on", st.name()))
}

fn hang(ctx: &ToolContext, args: &Json) -> ToolResult {
    if args.get("confirm").and_then(|v| v.as_str()) != Some("hang") {
        return Err(ToolError::BadArgument(String::from("hang stops the machine on purpose: pass confirm=\"hang\"")));
    }
    let mode = args.get("mode").and_then(|v| v.as_str()).unwrap_or("all");
    let Some(mode) = system::HangMode::parse(mode) else {
        return Err(ToolError::BadArgument(String::from("mode is all, one or halt")));
    };
    if let Some(why) = hang_refusal(system::state()) {
        return Err(ToolError::Refused(why));
    }
    let timeout = system::system().map_or(0, |d| d.timeout_s());
    klog::event!(Warn, "watchdog", "hang.requested", mode = mode.name(), timeout_s = timeout; "a deliberate hang follows the answer");
    if !ctx.after_answer(Box::new(move || system::deliberate_hang(mode))) {
        return Err(ToolError::Failed(String::from("hang runs only after its answer, and this call has no answer to wait for")));
    }
    Ok(vec![ToolOutput::Text(format!(
        "hanging once this answer is out (mode {}): the CPUs stop with interrupts off, the kicks stop, and the watchdog resets the machine \
         about {timeout} s later; the boot order brings V2 back and the next boot logs `watchdog fired`",
        mode.name()
    ))])
}

mcp_tool!(
    name: "hang",
    owner: "RB",
    safety: Change,
    description: "Hangs the machine on purpose after answering (confirm=\"hang\" required; refused unless the watchdog is armed): every CPU spins with interrupts off, so only the FCH watchdog brings it back. For Georg's A1 test: reset, back in V2 by itself, `watchdog fired` in the next boot's log.",
    schema: r#"{"type":"object","properties":{"confirm":{"type":"string","description":"must be \"hang\""},"mode":{"type":"string","enum":["all","one","halt"],"description":"all (default): every CPU spins; one: only the CPU running the tool; halt: every CPU halts"}},"required":["confirm"]}"#,
    handler: hang,
);
