//! MCP tool `input` (owner IN, Read): the input devices with their capabilities, the bound HID interfaces, the
//! held keys and lock LEDs, and the recent input events (taken from the sink's look-back window, never from
//! the console's reader). `since` + `wait_ms` let a caller press keys and watch them arrive (the Mini-PC check
//! list and host/tests/in).

use crate::class;
use crate::iface;
use crate::leds;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use console::evdev::{self, InputEvent};
use core::sync::atomic::Ordering;
use mcp::{Json, ToolContext, ToolError, ToolOutput, ToolResult};

/// Most events one answer lists (a bound on one request, not a count of things).
const LIST_MAX: u64 = 1000;
const WAIT_MAX_MS: u64 = 60_000;
/// Longest report-rate window (`rate_ms`).
const RATE_MAX_MS: u64 = 10_000;

fn arg_u64(args: &Json, key: &str) -> Result<Option<u64>, ToolError> {
    match args.get(key) {
        None => Ok(None),
        Some(v) => v.as_u64().map(Some).ok_or_else(|| ToolError::BadArgument(format!("{key}: want a non-negative integer"))),
    }
}

fn kind_name(k: u16) -> String {
    match k {
        evdev::EV_SYN => String::from("SYN"),
        evdev::EV_KEY => String::from("KEY"),
        evdev::EV_REL => String::from("REL"),
        evdev::EV_ABS => String::from("ABS"),
        evdev::EV_MSC => String::from("MSC"),
        evdev::EV_LED => String::from("LED"),
        k => format!("{k:#x}"),
    }
}

fn line(seq: u64, e: &InputEvent) -> String {
    let dev = if e.injected { format!("{:#x}", e.device) } else { format!("{}", e.device) };
    format!("ev {seq} t={}.{:06} dev={dev} {} code={} value={}{}", e.time_ns / 1_000_000_000, e.time_ns / 1000 % 1_000_000,
        kind_name(e.kind), e.code, e.value, if e.injected { " injected" } else { "" })
}

fn set<T: core::fmt::Display>(it: impl Iterator<Item = T>) -> String {
    let v: Vec<String> = it.map(|x| format!("{x}")).collect();
    format!("[{}]", v.join(","))
}

/// The state part of the answer.
pub fn describe() -> Vec<String> {
    let sink = evdev::sink();
    let mut out = Vec::new();
    let devs = evdev::devices();
    out.push(format!("input devices n={}", devs.len()));
    for d in &devs {
        let c = &d.info.caps;
        let abs = set(c.abs.iter().map(|(k, a)| format!("{k}:{}..{}", a.min, a.max)));
        out.push(format!(
            "device {} name=\"{}\" phys={} id={:04x}:{:04x} keyboard={} pointer={} keys={} rel={} abs={abs} leds={} repeat={} events={} held={}",
            d.number, d.info.name, d.info.phys, d.info.vendor, d.info.product, c.is_keyboard(), c.is_pointer(), c.keys.len(),
            set(c.rel.iter()), set(c.leds.iter()), c.repeat, d.events(), set(d.held_keys().iter())
        ));
    }
    for h in iface::all() {
        let status = match h.leds.status.load(Ordering::Acquire) {
            leds::STATUS_OK => "ok",
            leds::STATUS_FAILED => "failed",
            _ => "none",
        };
        out.push(format!(
            "hid {} interface={} protocol={} ep={:#04x} out_ep={} report_len={} inputs={} pipe={} restarts={} reports={} bad_reports={} errors={} leds={} led_failures={} pad={} binterval={} poll_us={} pad_poll_want_us={}",
            h.dev.path(), h.iface, h.protocol.as_str(), h.ep_in, h.out_ep.map(|e| format!("{e:#04x}")).unwrap_or_else(|| String::from("none")),
            h.report_len, set(h.numbers().iter()), iface::pipe_state_name(h.pipe_state.load(Ordering::Acquire)),
            h.restarts.load(Ordering::Relaxed), h.reports.load(Ordering::Relaxed), h.bad_reports.load(Ordering::Relaxed),
            h.errors.load(Ordering::Relaxed), if h.leds.present() { status } else { "absent" }, h.leds.failures.load(Ordering::Relaxed),
            h.pad, h.b_interval, class::poll_us(h.dev.speed(), h.b_interval),
            class::poll_want_us(h.dev.speed(), h.poll_want)
        ));
    }
    for (d, i, why) in iface::refusals() {
        out.push(format!("refused {} interface={i} id={:04x}:{:04x} reason=\"{why}\"", d.path(), d.vendor(), d.product()));
    }
    let locks = sink.locks();
    let on = |b: u16| if locks >> b & 1 != 0 { "on" } else { "off" };
    out.push(format!(
        "locks caps={} num={} scroll={} held={} physical_held={} dropped={} next_seq={}",
        on(evdev::LED_CAPSL), on(evdev::LED_NUML), on(evdev::LED_SCROLLL), set(sink.held_keys().iter()),
        set(sink.held_keys().into_iter().filter(|&k| sink.key_held_physical(k))), sink.dropped(), sink.next_seq()
    ));
    out
}

/// Reports per second of every bound interface over `ms` (the Mini-PC's gamepad check: 897-902 at 1 ms). A
/// bounded wait (B4): it ends at the window's end or on `stop`.
fn rates(ctx: &ToolContext, ms: u64) -> Vec<String> {
    let before: Vec<_> = iface::all().into_iter().map(|h| (h.reports.load(Ordering::Relaxed), h)).collect();
    let start = ctx.now_ns();
    let end = start.saturating_add(ms * 1_000_000);
    while ctx.now_ns() < end && !ctx.stop_requested() {
        sched::sleep_ns(end.saturating_sub(ctx.now_ns()).min(50_000_000));
    }
    let took = ctx.now_ns().saturating_sub(start).max(1);
    before
        .into_iter()
        .map(|(n, h)| {
            let d = h.reports.load(Ordering::Relaxed).saturating_sub(n);
            let centi = d.saturating_mul(100_000_000_000) / took;
            format!("rate {} interface={} reports={d} ms={} per_s={}.{:02}{}", h.dev.path(), h.iface, took / 1_000_000,
                centi / 100, centi % 100, if h.dev.is_gone() { " gone" } else { "" })
        })
        .collect()
}

fn input(ctx: &ToolContext, args: &Json) -> ToolResult {
    let since = arg_u64(args, "since")?;
    let wait_ms = arg_u64(args, "wait_ms")?.unwrap_or(0);
    let max = arg_u64(args, "max")?.unwrap_or(200).clamp(1, LIST_MAX) as usize;
    if wait_ms > WAIT_MAX_MS {
        return Err(ToolError::BadArgument(format!("wait_ms: at most {WAIT_MAX_MS}")));
    }
    let rate_ms = arg_u64(args, "rate_ms")?;
    if rate_ms.is_some_and(|r| r == 0 || r > RATE_MAX_MS) {
        return Err(ToolError::BadArgument(format!("rate_ms: 1 to {RATE_MAX_MS}")));
    }
    let rates = rate_ms.map(|ms| rates(ctx, ms));
    let sink = evdev::sink();
    if let Some(s) = since
        && wait_ms > 0
    {
        let deadline = ctx.now_ns().saturating_add(wait_ms * 1_000_000);
        // Bounded (B4): MCP workers are Normal threads, the wait ends at the deadline or on `stop`.
        while sink.next_seq() <= s && !ctx.stop_requested() {
            let left = deadline.saturating_sub(ctx.now_ns());
            if left == 0 {
                break;
            }
            let _ = sink.wait_queue().wait_until_timeout(|| sink.next_seq() > s, left.min(100_000_000));
        }
    }
    let mut out = describe();
    out.extend(rates.into_iter().flatten());
    if let Some(s) = since {
        let ev = sink.recent_since(s);
        let first = ev.first().map(|(q, _)| *q).unwrap_or(sink.next_seq());
        if first > s {
            out.push(format!("events lost before {first} (look-back window)"));
        }
        out.push(format!("events n={} shown={}", ev.len(), ev.len().min(max)));
        for (q, e) in ev.iter().take(max) {
            out.push(line(*q, e));
        }
    }
    Ok(alloc::vec![ToolOutput::Text(out.join("\n"))])
}

mcp::mcp_tool!(
    name: "input",
    owner: "IN",
    safety: Read,
    description: "Input devices (keyboards, mice, tablets) with their capabilities, the bound USB HID interfaces (protocol, reports, errors, LED state), held keys and Caps/Num/Scroll Lock. since: also list the input events from this sequence number on (next_seq is in the answer); wait_ms: wait up to this long for an event after since; rate_ms: count each interface's reports over this window and give reports per second (gamepads: about 900 at 1 ms polling). Shows typed keys: use it for checks, not while a secret is typed.",
    schema: r#"{"type":"object","properties":{"since":{"type":"integer","minimum":0},"wait_ms":{"type":"integer","minimum":0,"maximum":60000},"max":{"type":"integer","minimum":1,"maximum":1000},"rate_ms":{"type":"integer","minimum":1,"maximum":10000}}}"#,
    handler: input,
);
