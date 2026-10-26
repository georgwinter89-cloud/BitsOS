//! MCP `wlan` (B5: owner WL, safety change): `status` (default) shows each card's state, SSID,
//! channel, signal, rates, its address from the stack, the counters and the last error from the
//! thread's status copy (never waits for the chip); `scan`, `join` and `leave` go to the driver
//! thread and wait for its answer at most until the call's deadline (B4; a join itself runs in
//! the thread, `status` follows it). The passphrase is never an argument and never in an answer
//! (R8 i): it comes only from CONFIG.TXT.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use mcp::{Json, ToolContext, ToolError, ToolOutput, ToolResult};
use rtw89_core::drv::runner::{SCAN_CHANNELS_2G, SCAN_DWELL_MS};

use crate::card::{Card, Cmd};

/// A command waits at most this long for the thread (a scan of 13 channels at 120 ms is ~1.6 s)
pub const CMD_WAIT_NS: u64 = 30_000_000_000;

/// The longest scan one call may ask for (channels x dwell): the thread holds the chip that long
/// and serves nothing else (13 channels at 1 s still fit)
pub const SCAN_MAX_MS: u64 = 15_000;

/// Arguments that would carry a secret: refused before anything else
pub const SECRET_ARGS: [&str; 4] = ["psk", "wlan_psk", "passphrase", "password"];

fn ip_of(name: &str) -> (String, bool) {
    match net::stack::kernel() {
        Some(s) => {
            let list = s.interfaces();
            match list.iter().find(|i| i.name == name) {
                Some(i) => (
                    i.info.addr.map_or(String::from("-"), |(a, p)| {
                        let [a, b, c, d] = a.0;
                        format!("{a}.{b}.{c}.{d}/{p}")
                    }),
                    i.default_route,
                ),
                None => (String::from("- (not registered with the stack yet)"), false),
            }
        }
        None => (String::from("- (no network stack)"), false),
    }
}

/// The status text of one card
pub fn status_lines(c: &Card) -> Vec<String> {
    let (ip, default_route) = ip_of(&c.name);
    let phase = c.phase.lock().clone();
    let mut out = alloc::vec![format!(
        "{} function={} thread={} netdev={} link={}{}",
        c.name,
        c.function,
        phase,
        if c.registered.load(Ordering::Acquire) { "registered" } else { "not registered" },
        if c.link_up.load(Ordering::Acquire) { "up" } else { "down" },
        if default_route { " default-route" } else { "" }
    )];
    out.extend(c.status.lock().lines(&ip));
    let irq = match c.irqs.lock().as_ref() {
        Some(set) => {
            let st = set.stats(0);
            format!(
                "irq=msi deliveries={} stormed={}",
                c.irq_count.load(Ordering::Relaxed),
                st.is_some_and(|x| x.stormed)
            )
        }
        None => String::from("irq=polled"),
    };
    out.push(irq);
    out.push(format!(
        "queues tx_queued={} tx_refused={} tx_no_link={} rx_taken={} rx_overflow={} txq={} rxq={}",
        c.tx_queued.load(Ordering::Relaxed),
        c.tx_refused.load(Ordering::Relaxed),
        c.tx_no_link.load(Ordering::Relaxed),
        c.rx_taken.load(Ordering::Relaxed),
        c.rx_overflow.load(Ordering::Relaxed),
        c.txq.lock().len(),
        c.rxq.lock().len()
    ));
    out
}

fn bad(what: &str) -> ToolError {
    ToolError::BadArgument(String::from(what))
}

/// The arguments as a command (`None`: status)
pub fn parse(args: &Json) -> Result<Option<Cmd>, ToolError> {
    for k in SECRET_ARGS {
        if args.get(k).is_some() {
            return Err(ToolError::Refused(String::from(
                "the WLAN passphrase comes only from CONFIG.TXT (wlan_psk=), never over MCP (R8 i)",
            )));
        }
    }
    let action = match args.get("action") {
        None => "status",
        Some(a) => a.as_str().ok_or_else(|| bad("action"))?,
    };
    Ok(match action {
        "status" => None,
        "scan" => {
            let channels = match args.get("channels") {
                None => SCAN_CHANNELS_2G.to_vec(),
                Some(c) => {
                    let list = c.as_array().ok_or_else(|| bad("channels"))?;
                    let mut v: Vec<u32> = Vec::new();
                    for x in list {
                        let n = x.as_u64().filter(|&n| (1..=13).contains(&n) || (36..=165).contains(&n));
                        let n = n.ok_or_else(|| bad("channels"))? as u32;
                        if !v.contains(&n) {
                            v.push(n);
                        }
                    }
                    // one band, as wlan_channels= in CONFIG.TXT
                    if v.is_empty() || !(v.iter().all(|&c| c <= 14) || v.iter().all(|&c| c > 14)) {
                        return Err(bad("channels (one band: 1-13 or 36-165)"));
                    }
                    v
                }
            };
            let dwell_ms = match args.get("dwell_ms") {
                None => SCAN_DWELL_MS,
                Some(d) => d.as_u64().filter(|d| (10..=1000).contains(d)).ok_or_else(|| bad("dwell_ms"))? as u32,
            };
            if channels.len() as u64 * u64::from(dwell_ms) > SCAN_MAX_MS {
                return Err(bad("channels x dwell_ms is over 15 s"));
            }
            Some(Cmd::Scan { channels, dwell_ms })
        }
        "join" => Some(Cmd::Join),
        "leave" => Some(Cmd::Leave),
        _ => return Err(bad("action")),
    })
}

pub(crate) fn wlan(ctx: &ToolContext, args: &Json) -> ToolResult {
    let cmd = parse(args)?;
    let cards: Vec<Arc<Card>> = crate::cards();
    if cards.is_empty() {
        let why = if pci::registry::opted_in("wlan") {
            "no RTL8852BE (10EC:B852) was found"
        } else {
            "wlan=on is not in CONFIG.TXT: the WLAN chip and its bridge stay untouched (R8 d)"
        };
        return Ok(alloc::vec![ToolOutput::Text(format!("wlan: no card ({why})"))]);
    }
    let Some(cmd) = cmd else {
        let mut out = Vec::new();
        for c in &cards {
            out.extend(status_lines(c));
        }
        return Ok(alloc::vec![ToolOutput::Text(out.join("\n"))]);
    };
    let wait = ctx.remaining_ns().clamp(1_000_000_000, CMD_WAIT_NS);
    match cards[0].command(cmd, wait) {
        Ok(lines) => Ok(alloc::vec![ToolOutput::Text(lines.join("\n"))]),
        Err(e) => Err(ToolError::Failed(e)),
    }
}

mcp::mcp_tool!(
    name: "wlan",
    owner: "WL",
    safety: Change,
    description: "The WLAN card (RTL8852BE, attached only with wlan=on in CONFIG.TXT). action: status (default; state, SSID, BSSID, channel, signal dBm, rates, IP, default route, counters, last error, never waits), scan (channels: list of one band, default 1-13; dwell_ms 10-1000, default 120; at most 15 s in all; only without a link), join (the network of wlan_ssid=/wlan_psk= in CONFIG.TXT, the supervisor rejoins by itself), leave (no rejoin until join). The passphrase is never an argument and never shown. The cable stays the lifeline: the WLAN never takes it down.",
    schema: r#"{"type":"object","properties":{"action":{"type":"string","enum":["status","scan","join","leave"]},"channels":{"type":"array","items":{"type":"integer","minimum":1,"maximum":165}},"dwell_ms":{"type":"integer","minimum":10,"maximum":1000}}}"#,
    handler: wlan,
);
