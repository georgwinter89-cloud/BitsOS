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

/// `device`: which GPU (`devs::Registry::pick`), or `list`. Without it the first device answers, which is the 780M on
/// the Mini-PC: every call of before means what it meant.
pub fn parse_device(args: &Json) -> Result<Option<String>, ToolError> {
    match args.get("device").filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v.as_str().map(|s| Some(String::from(s))).ok_or_else(|| ToolError::BadArgument(String::from("device (card0, 04:00.0 or list)"))),
    }
}

/// One line per device, in the stable order.
pub fn list(reg: &crate::devs::Registry) -> String {
    let mut s = format!("gpu devices: {}\n", reg.all().len());
    for d in reg.all() {
        s.push_str(&format!(
            "{} {} renderD{}: {}{}\n",
            d.card_name(),
            d.key,
            d.render_minor(),
            match d.family {
                crate::devs::Family::Phoenix => "Radeon 780M driver (driven)",
                crate::devs::Family::Rdna4 => "Radeon RX 9070 XT (read-only attach, GP-GPU1)",
            },
            if d.display { ", the boot display" } else { "" }
        ));
    }
    s
}

fn gpu(_ctx: &ToolContext, args: &Json) -> ToolResult {
    let want = parse_device(args)?;
    let reg = crate::devs::registry();
    if want.as_deref() == Some("list") {
        return Ok(vec![ToolOutput::Text(list(reg))]);
    }
    let dev = match want.as_deref() {
        None => reg.all().first().copied(),
        Some(a) => Some(reg.pick(Some(a)).map_err(ToolError::BadArgument)?),
    };
    match dev {
        Some(d) if d.family == crate::devs::Family::Rdna4 => crate::rdna4::live::answer(d).map(|s| vec![ToolOutput::Text(s)]).map_err(ToolError::Failed),
        // The 780M's device, and a machine without any device (QEMU): the answer of before.
        Some(d) => answer(d.hw.node.gpu().and_then(|g| g.dpm())),
        None => answer(None),
    }
}

mcp::mcp_tool!(
    name: "gpu",
    owner: "GX",
    safety: Read,
    description: "One GPU device's state; device=list names them (card0 is the boot display, then by PCI address). Radeon 780M: power and clocks (GX-9b), read from PMFW's metrics table through one table transfer on the driver's mailbox; no GC register is touched, so a sleeping GFX block sleeps on. Lines: 'level:' (auto with PMFW's GFX range, or why it was left), 'clocks:' (gfx, its average, soc, mem in MHz), 'activity:' (%), 'temperature:' (C), 'power:' (W), 'voltage:' (mV), 'throttle:', 'metrics counter:', 'gfxoff:' (allowed, held off, off/refused; the request count; power gating; messages sent), 'gfxoff status:' (SMUIO's status, unverified on 13.0.4). Without a backend on the render node: an error. Radeon RX 9070 XT (GP-GPU1, read-only attach: the attach writes nothing to the card; only once the stage pmfw passed in this boot do the temperature lines come from Linux's one metrics message, TransferTableSmu2Dram with argument 5: three writes of the SMU's mailbox registers, then the PMFW writes its table into VRAM; never during a stage call): 'gpu cardN <address>:' (ids, whether it is the display card), 'writes:' (must say nothing was written), 'pci:' (command, power state, MSI), 'bar0/bar2/bar5:', 'link:' (the card's and the narrowest above it), 'dma:', 'attach:' (PASS, or STOPPED at step N of 5 with the reason), 'memsize:', 'discovery:', 'measured:' (the table against the one read under Linux), 46 'ip ...' lines, 12 'module <role>:' lines (which code is not built yet and which item brings it), and a 'firmware <file>:' line per file the card needs (present on the stick with size and header versions, or missing).",
    schema: r#"{"type":"object","properties":{"device":{"type":"string","description":"card0, card1, a PCI address (04:00.0), or list; default: the first device"}}}"#,
    handler: gpu,
);
