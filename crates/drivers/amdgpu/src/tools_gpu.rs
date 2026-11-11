//! GX-9b's MCP tool `gpu` (class read): clocks, GFX activity, temperature and power as far as PMFW's metrics table gives
//! them, the clock level and GFXOFF's state. It sends one table transfer to PMFW and touches no GC register, so it never
//! wakes a sleeping block. The chain is reached through its bounded lock only.

use crate::dpm::{Dpm, Level, Report};
use crate::dpm_table::{centi, milli, throttler_names};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use mcp::{Json, ToolContext, ToolError, ToolOutput, ToolResult};

/// SMUIO_GFX_MISC_CNTL's PWR_GFXOFF_STATUS (bits 2:1; smu13.rs: unverified on 13.0.4 until the Mini-PC shows it).
fn smuio_line(v: Option<u32>) -> String {
    match v {
        None => String::from("no read"),
        Some(v) => {
            let s = (v >> 1) & 3;
            let name = ["in GFXOFF", "leaving", "powered", "entering"][s as usize];
            format!("{s} {name} (register {v:#x}; its meaning is unverified on SMU 13.0.4)")
        }
    }
}

/// The answer's text.
pub fn text(r: &Report) -> String {
    let mut s = String::from("gpu: 780M, SMU 13.0.4, PMFW's metrics table\n");
    match &r.level {
        Level::NotStarted => s.push_str("level: not started\n"),
        Level::Auto(g) => s.push_str(&format!("level: auto, gfx {}..{} MHz from PMFW's table\n", g.min, g.max)),
        Level::Left(why) => s.push_str(&format!("level: left as PMFW has it ({why})\n")),
    }
    match &r.metrics {
        Ok(m) => {
            s.push_str(&format!("clocks: gfx {} MHz (average {}), soc {} MHz, mem {} MHz\n", m.gfx_mhz, m.avg_gfx_mhz, m.soc_mhz, m.mem_mhz));
            s.push_str(&format!("activity: gfx {} % (average {} %)\n", centi(m.gfx_activity_centi), centi(m.avg_gfx_activity_centi)));
            s.push_str(&format!("temperature: gfx {} C, soc {} C\n", centi(m.gfx_temp_centi), centi(m.soc_temp_centi)));
            s.push_str(&format!("power: socket {} W (average {}), apu {} W\n", milli(m.socket_mw), milli(m.avg_socket_mw), milli(m.apu_mw)));
            s.push_str(&format!("voltage: vdd {} mV, soc {} mV\n", m.vdd_mv, m.soc_mv));
            s.push_str(&format!("throttle: {}\n", throttler_names(m.throttler)));
            s.push_str(&format!("metrics counter: {}\n", m.counter));
        }
        Err(why) => s.push_str(&format!("metrics: FAILED ({why})\n")),
    }
    let state = if r.dead {
        "PMFW silent, no messages until its answer shows"
    } else if r.asleep {
        "allowed"
    } else if r.no_allow {
        "off, refused"
    } else {
        "held off"
    };
    s.push_str(&format!(
        "gfxoff: {state}; requests {}; power gating {}; allow {}, disallow {}, failed messages {}\n",
        r.requests,
        if r.pg_on { "on" } else { "off" },
        r.allows,
        r.disallows,
        r.failures
    ));
    s.push_str(&format!("gfxoff status: {}\n", smuio_line(r.smuio)));
    s
}

/// The tool on a backend, or without one.
pub fn answer(dpm: Option<Arc<Dpm>>) -> ToolResult {
    let d = dpm.ok_or_else(|| ToolError::Failed(String::from("no backend on the render node (gx_bringup until=backend; QEMU has none)")))?;
    Ok(vec![ToolOutput::Text(text(&d.report()))])
}

fn gpu(_ctx: &ToolContext, _args: &Json) -> ToolResult {
    answer(crate::hw::live::NODE.gpu().and_then(|g| g.dpm()))
}

mcp::mcp_tool!(
    name: "gpu",
    owner: "GX",
    safety: Read,
    description: "Radeon 780M power and clocks (GX-9b), read from PMFW's metrics table through one table transfer on the driver's mailbox; no GC register is touched, so a sleeping GFX block sleeps on. Lines: 'level:' (auto with PMFW's GFX range, or why it was left), 'clocks:' (gfx, its average, soc, mem in MHz), 'activity:' (%), 'temperature:' (C), 'power:' (W), 'voltage:' (mV), 'throttle:', 'metrics counter:', 'gfxoff:' (allowed, held off, off/refused; the request count; power gating; messages sent), 'gfxoff status:' (SMUIO's status, unverified on 13.0.4). Without a backend on the render node: an error.",
    schema: r#"{"type":"object","properties":{}}"#,
    handler: gpu,
);
