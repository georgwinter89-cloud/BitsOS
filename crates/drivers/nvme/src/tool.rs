//! MCP tool `nvme` (Read): controllers (PCI address, model, serial, firmware, state, interrupts, command
//! counts with `media_writes`, which is 0 by construction in NV-1), namespaces with their disks, partition
//! tables and write verdicts; `smart=true` adds the SMART / Health log; `speed_mb=n` reads n MiB of one
//! namespace into the driver's buffer and reports the rate (reads only).

use crate::{Nvme, controllers};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write;
use mcp::{Json, ToolContext, ToolError, ToolOutput, ToolResult, mcp_tool};

/// Largest `speed_mb` (a single call's bound, about two seconds on a fast SSD).
const SPEED_MAX_MB: u64 = 4096;

fn ops(list: &[(u8, u64)]) -> String {
    if list.is_empty() {
        return String::from("-");
    }
    list.iter().map(|(o, n)| format!("{o:02x}h={n}")).collect::<Vec<_>>().join(" ")
}

pub fn lines(c: &Arc<Nvme>, smart: bool) -> Vec<String> {
    let mut out = Vec::new();
    let i = c.info();
    let pci = c.hw.pci();
    let mut l = format!("{} at {} {:04x}:{:04x} state={}", c.name, pci.loc(), pci.vendor, pci.device, c.state().name());
    if let Some(id) = &i.ident {
        let _ = write!(l, " model=\"{}\" serial=\"{}\" firmware=\"{}\" cntlid={} mdts={} cache={}", id.model, id.serial, id.firmware, id.cntlid, id.mdts, id.volatile_cache());
    }
    let _ = write!(
        l,
        " version={} irq={} vectors={} io_depth={} max_transfer={} starts={}",
        crate::regs::version_text(i.version),
        i.irq.map_or("-", |m| m.name()),
        i.irq.map_or(0, |m| m.vectors()),
        i.io_depth,
        i.max_transfer,
        i.starts
    );
    if let Some(e) = &i.last_error {
        let _ = write!(l, " last_error=\"{e}\"");
    }
    out.push(l);
    let s = &c.stats;
    let (adm, io) = s.used();
    let ld = |a: &core::sync::atomic::AtomicU64| a.load(core::sync::atomic::Ordering::Relaxed);
    out.push(format!(
        "  commands admin: {} | io: {} | media_writes={} errors={} timeouts={} resets={} stale={} bytes_read={}",
        ops(&adm),
        ops(&io),
        s.media_writes(),
        ld(&s.errors),
        ld(&s.timeouts),
        ld(&s.resets),
        ld(&s.stale),
        ld(&s.bytes_read)
    ));
    if let Some((mode, counts, released)) = c.hw.irq_stats() {
        out.push(format!("  vectors {:?} interrupts={:?} released={released}", mode, counts));
    }
    for ns in &i.namespaces {
        let mut l = format!("  ns{} blocks={} block_size={} formats={} flbas={:#x}", ns.nsid, ns.id.nsze, ns.block_size(), ns.id.formats.len(), ns.id.flbas);
        if let Some(why) = &ns.unusable {
            let _ = write!(l, " unusable=\"{why}\"");
        }
        let disk = format!("{}n{}", c.name, ns.nsid);
        match block::find(&disk) {
            Some(d) => {
                let scan = d.scan();
                let _ = write!(l, " disk={} table={} verdict={:?}", disk, scan.scheme_str(), scan.verdict());
                match d.write_policy() {
                    Ok(()) => l.push_str(" writes=allowed"),
                    Err(r) => {
                        let _ = write!(l, " writes=refused ({})", r.as_str());
                    }
                }
                out.push(l);
                for (first, blocks, kind, number) in &scan.parts {
                    out.push(format!("    {} first={first} blocks={blocks} type=\"{kind}\"", crate::gpt::part_name(&disk, *number)));
                }
            }
            None => {
                l.push_str(" disk=-");
                out.push(l);
            }
        }
    }
    if smart {
        match c.smart() {
            Ok(m) => out.push(format!(
                "  smart warning={:#x} temperature={}K ({}C) spare={}% threshold={}% used={}% units_read={} units_written={} host_reads={} host_writes={} power_cycles={} power_on_hours={} unsafe_shutdowns={} media_errors={} error_log_entries={}",
                m.critical_warning,
                m.temperature_k,
                m.temperature_k as i32 - 273,
                m.available_spare,
                m.spare_threshold,
                m.percent_used,
                m.data_units_read,
                m.data_units_written,
                m.host_reads,
                m.host_writes,
                m.power_cycles,
                m.power_on_hours,
                m.unsafe_shutdowns,
                m.media_errors,
                m.error_log_entries
            )),
            Err(e) => out.push(format!("  smart failed: {e}")),
        }
    }
    out
}

fn num(args: &Json, key: &str) -> Result<Option<u64>, ToolError> {
    match args.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(v) => v.as_u64().map(Some).ok_or_else(|| ToolError::BadArgument(format!("{key} must be a whole number"))),
    }
}

fn nvme(_ctx: &ToolContext, args: &Json) -> ToolResult {
    let smart = match args.get("smart") {
        None | Some(Json::Null) => false,
        Some(Json::Bool(b)) => *b,
        Some(_) => return Err(ToolError::BadArgument(String::from("smart must be true or false"))),
    };
    let want = args.get("controller").and_then(|v| v.as_str());
    let speed = num(args, "speed_mb")?;
    let nsid = num(args, "nsid")?.unwrap_or(1);
    let lba = num(args, "lba")?.unwrap_or(0);
    let _ = crate::settle(2_000_000_000);
    let all = controllers();
    let picked: Vec<Arc<Nvme>> = all.iter().filter(|c| want.is_none_or(|w| c.name == w)).cloned().collect();
    if picked.is_empty() {
        return match want {
            Some(w) => Err(ToolError::BadArgument(format!("no controller {w} (have: {})", all.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ")))),
            None => Ok(vec![ToolOutput::Text(String::from("no NVMe controller"))]),
        };
    }
    let mut out = Vec::new();
    if let Some(mb) = speed {
        if mb == 0 || mb > SPEED_MAX_MB {
            return Err(ToolError::BadArgument(format!("speed_mb must be 1..={SPEED_MAX_MB}")));
        }
        let c = &picked[0];
        let nsid = u32::try_from(nsid).map_err(|_| ToolError::BadArgument(String::from("nsid too large")))?;
        match c.read_speed(nsid, lba, mb << 20) {
            Ok((bytes, ns)) => out.push(format!(
                "{} ns{nsid} read {bytes} bytes from LBA {lba} in {} us = {} MB/s",
                c.name,
                ns / 1000,
                if ns == 0 { 0 } else { bytes * 1000 / ns }
            )),
            Err(e) => return Err(ToolError::Failed(format!("{} ns{nsid}: {}", c.name, e.text()))),
        }
    }
    for c in &picked {
        out.extend(lines(c, smart));
    }
    Ok(vec![ToolOutput::Text(out.join("\n"))])
}

mcp_tool!(
    name: "nvme",
    owner: "NV",
    safety: Read,
    description: "NVMe controllers (read-only driver, NV-1): PCI address, model, serial, firmware, state, interrupt mode, commands issued per opcode with media_writes (always 0: the driver has no write command), each namespace with its block disk, partition table, verdict (Windows/EFI = never written) and write policy, and its partitions. smart=true adds the SMART/Health log (temperature, wear, power-on hours, unsafe shutdowns, media errors). speed_mb=n (1..4096) reads n MiB from namespace nsid (default 1) at lba (default 0) of the first selected controller and reports MB/s.",
    schema: r#"{"type":"object","properties":{"controller":{"type":"string","description":"nvme0, nvme1, ... (default: all)"},"smart":{"type":"boolean","description":"read the SMART/Health log"},"speed_mb":{"type":"integer","description":"read this many MiB and report the rate"},"nsid":{"type":"integer","description":"namespace for speed_mb (default 1)"},"lba":{"type":"integer","description":"start block for speed_mb (default 0)"}}}"#,
    handler: nvme,
);
