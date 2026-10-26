//! PC-5 tool-level ktests: every call goes through `rpc::handle` with the kernel's real tool list, as an MCP
//! client's would (the server only adds the transport). The `Safe` ones read, or call paths that must refuse
//! before any access; on hardware a broken guard could at most write a read-only ID register. The pci-testdev
//! one writes and is QemuOnly (scenario `host/tests/pc/40-raw.sh` adds the device).

use super::HOSTILE;
use crate::{Json, Safety, TOOLS, rpc};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};
use pci::Loc;

ktest!("pc.tool.registry", Safe, registry);
ktest!("pc.tool.read_host_bridge", Safe, read_host_bridge);
ktest!("pc.tool.storage_refused", Safe, storage_refused);
ktest!("pc.tool.untouched_refused", Safe, untouched_refused);
ktest!("pc.tool.dcn_refused", Safe, dcn_refused);
ktest!("pc.tool.core_regs_refused", Safe, core_regs_refused);
ktest!("pc.tool.hostile_args", Safe, hostile_args);
ktest!("pc.tool.rescan", Safe, rescan);
ktest!("pc.tool.testdev", QemuOnly, testdev);

struct Reply {
    error: bool,
    text: String,
    rpc_error: Option<i64>,
}

fn call(name: &str, args: &str) -> Reply {
    let body = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#);
    let a = rpc::handle(body.as_bytes(), time::now_ns(), TOOLS.entries());
    let j = Json::parse(&a.body).unwrap_or(Json::Null);
    let r = j.get("result");
    let items = r.and_then(|r| r.get("content")).and_then(Json::as_array).unwrap_or(&[]);
    let text = items.iter().filter_map(|i| i.get("text").and_then(Json::as_str)).collect::<Vec<_>>().join("\n");
    Reply {
        error: r.and_then(|r| r.get("isError")).and_then(Json::as_bool) == Some(true),
        text,
        rpc_error: j.get("error").and_then(|e| e.get("code")).and_then(Json::as_i64),
    }
}

fn ok(name: &str, args: &str) -> Result<String, String> {
    let r = call(name, args);
    if r.error || r.rpc_error.is_some() {
        return Err(format!("{name} {args}: {:?} {}", r.rpc_error, r.text));
    }
    Ok(r.text)
}

fn refused(name: &str, args: &str, why: &str) -> TestResult {
    let r = call(name, args);
    kassert!(r.error && r.text.starts_with("Refused: ") && r.text.contains(why), "{name} {args}: not refused for '{why}': {:?} {}", r.rpc_error, r.text);
    Ok(())
}

/// The hex values of a read answer, in order (every line after the first; untouched note skipped).
fn values(text: &str) -> Vec<u64> {
    text.lines()
        .skip(1)
        .filter_map(|l| l.split_once(": ").filter(|(a, _)| a.starts_with("0x")).map(|(_, v)| v))
        .flat_map(|v| v.split(' ').filter_map(|x| u64::from_str_radix(x, 16).ok()))
        .collect()
}

fn loc(at: Loc) -> String {
    format!("{at}")
}

fn registry() -> TestResult {
    for (name, safety) in [("pci_read", Safety::Read), ("pci_write", Safety::Change), ("mmio_read", Safety::Read), ("mmio_write", Safety::Change), ("pci_rescan", Safety::Change)] {
        let t = crate::find(name).ok_or_else(|| format!("{name} missing"))?;
        kassert!(t.owner == "PC" && t.safety == safety, "{name}: owner {} safety {:?}", t.owner, t.safety);
    }
    Ok(())
}

/// 00:00.0 read through the tool equals what the scan found, at widths 4, 2 and 1. On q35 the MCH is untouched
/// (the contract test's key), so only its header is readable and a read past it is refused; elsewhere the
/// whole space is.
fn read_host_bridge() -> TestResult {
    let at = Loc::new(0, 0, 0, 0);
    let f = pci::function(at).ok_or("no 00:00.0 in the tree")?;
    let id = (f.device as u64) << 16 | f.vendor as u64;
    let v = values(&ok("pci_read", r#"{"function":"00:00.0","offset":0}"#)?);
    kassert!(v == [id], "width 4: {v:x?}, scan {id:#x}");
    let v = values(&ok("pci_read", r#"{"function":"00:00.0","offset":0,"width":2,"count":2}"#)?);
    kassert!(v == [f.vendor as u64, f.device as u64], "width 2: {v:x?}");
    let v = values(&ok("pci_read", r#"{"function":"0000:00:00.0","offset":"0x2","width":1,"count":2}"#)?);
    kassert!(v == [f.device as u64 & 0xFF, f.device as u64 >> 8], "width 1: {v:x?}");
    let t = ok("pci_read", r#"{"function":"00:00.0","offset":0,"count":16}"#)?;
    kassert!(values(&t).len() == 16 && t.lines().count() >= 5, "a whole header: {t}");
    let class = values(&ok("pci_read", r#"{"function":"00:00.0","offset":8}"#)?);
    kassert!(class.first().is_some_and(|c| (c >> 8) as u32 == f.class), "class {class:x?} vs {:#x}", f.class);
    match f.untouched() {
        Some(_) => refused("pci_read", r#"{"function":"00:00.0","offset":64}"#, "untouched")?,
        None => kassert!(ok("pci_read", r#"{"function":"00:00.0","offset":64}"#).is_ok()),
    }
    Ok(())
}

/// Every class 01 function (q35: the AHCI at 00:1f.2; Mini-PC: both NVMe): pci_write (to the read-only ID
/// register, with the ID as value), mmio_read and mmio_write (offset 0 of each memory BAR: a capability
/// register, read-only) refused; the ID unchanged. Every bridge with storage below it: pci_write refused.
fn storage_refused() -> TestResult {
    let all = pci::devices();
    let disks: Vec<_> = all.iter().filter(|f| f.class >> 16 == 0x01).collect();
    if disks.is_empty() {
        return ktest::skip("no mass storage function on this machine");
    }
    for f in disks {
        let at = loc(f.loc());
        let id = values(&ok("pci_read", &format!(r#"{{"function":"{at}","offset":0}}"#))?);
        kassert!(id.len() == 1, "{at}: id {id:?}");
        refused("pci_write", &format!(r#"{{"function":"{at}","offset":0,"value":{}}}"#, id[0]), "R8a")?;
        for b in 0..6u8 {
            if f.bar(b).is_some() {
                refused("mmio_read", &format!(r#"{{"function":"{at}","bar":{b},"offset":0}}"#), "R8a")?;
                refused("mmio_write", &format!(r#"{{"function":"{at}","bar":{b},"offset":0,"value":0}}"#), "R8a")?;
            }
        }
        let again = values(&ok("pci_read", &format!(r#"{{"function":"{at}","offset":0}}"#))?);
        kassert!(again == id, "{at}: ID changed");
        for b in f.upstream_bridges() {
            let bl = loc(b.loc());
            if b.untouched().is_none() {
                refused("pci_write", &format!(r#"{{"function":"{bl}","offset":0,"value":0}}"#), "bridge with mass storage")?;
            }
        }
    }
    Ok(())
}

/// Every untouched function (Mini-PC: 04:00.0 and 00:02.2 until wlan=on; q35: the MCH and the SMBus with
/// test keys): the command register readable and equal to the scan's (PC-4 check list item 1), a read past
/// the header, every write and every BAR access refused, the command register unchanged afterwards.
fn untouched_refused() -> TestResult {
    let nodes: Vec<pci::Node> = pci::tree().into_iter().filter(|n| n.untouched.is_some()).collect();
    if nodes.is_empty() {
        return ktest::skip("no untouched function on this machine");
    }
    for n in nodes {
        let at = loc(n.loc);
        let cmd = values(&ok("pci_read", &format!(r#"{{"function":"{at}","offset":4,"width":2}}"#))?);
        kassert!(cmd == [n.command_at_scan as u64], "{at}: command {cmd:x?}, at scan {:#x}", n.command_at_scan);
        refused("pci_read", &format!(r#"{{"function":"{at}","offset":64}}"#), "untouched")?;
        refused("pci_write", &format!(r#"{{"function":"{at}","offset":4,"width":2,"value":{}}}"#, n.command_at_scan), "untouched")?;
        refused("mmio_read", &format!(r#"{{"function":"{at}","bar":0,"offset":0}}"#), "untouched")?;
        refused("mmio_write", &format!(r#"{{"function":"{at}","bar":2,"offset":0,"value":0}}"#), "untouched")?;
        let after = values(&ok("pci_read", &format!(r#"{{"function":"{at}","offset":4,"width":2}}"#))?);
        kassert!(after == cmd, "{at}: command changed");
        klog::event!(Info, "pci", "ktest.untouched_tool", function = at.as_str(), command = klog::Hex(cmd[0]));
    }
    Ok(())
}

/// The Radeon's BAR5 index/data pairs are refused by the tool's own guard (only their owner, under its lock);
/// the made-up Radeon of `pc.raw.guards` covers machines without one. Only the guard is asked (pure, no
/// access): a broken guard must never touch a display register in a Safe test.
fn dcn_refused() -> TestResult {
    let gpus: Vec<pci::Node> = pci::tree().into_iter().filter(pci::raw::is_amd_display).collect();
    for g in &gpus {
        let r = pci::raw::mmio_check(g, pci::raw::AMD_REG_BAR, 0x38, 4, 1, true);
        kassert!(r == Err(pci::raw::Refusal::IndexPair), "{}: {r:?}", g.loc);
    }
    if gpus.is_empty() {
        return ktest::skip("no Radeon here (the made-up one is checked by pc.raw.guards)");
    }
    Ok(())
}

/// Every function a raw write may reach (touched, no mass storage at or below it): pci_write to BAR0 and to
/// the MSI / MSI-X Message Control, each with the value it has now, is refused and the value stays. On
/// hardware a broken guard would at most write a register back with its own value.
fn core_regs_refused() -> TestResult {
    let nodes = pci::tree();
    let mut seen = 0;
    for (i, n) in nodes.iter().enumerate() {
        if n.untouched.is_some() || n.is_storage() || (n.is_bridge() && pci::raw::storage_below(&nodes, i)) {
            continue;
        }
        let at = loc(n.loc);
        let mut targets = alloc::vec![(0x10u64, 4u8, "BARs, the ROM BAR")];
        for id in [pci::caps::id::MSI, pci::caps::id::MSIX] {
            if let Some(c) = n.cap(id) {
                targets.push((c.offset as u64 + 2, 2, "MSI and MSI-X"));
            }
        }
        for (off, width, why) in targets {
            let rd = format!(r#"{{"function":"{at}","offset":{off},"width":{width}}}"#);
            let now = values(&ok("pci_read", &rd)?);
            kassert!(now.len() == 1, "{at} {off:#x}: {now:?}");
            refused("pci_write", &format!(r#"{{"function":"{at}","offset":{off},"width":{width},"value":{}}}"#, now[0]), why)?;
            kassert!(values(&ok("pci_read", &rd)?) == now, "{at} {off:#x} changed");
            seen += 1;
        }
    }
    if seen == 0 {
        return ktest::skip("no function a raw write may reach");
    }
    klog::event!(Info, "pci", "ktest.core_regs", refused = seen);
    Ok(())
}

/// Malformed arguments: invalid params (-32602) or a refusal, never a panic and never an access.
fn hostile_args() -> TestResult {
    for (tool, args) in HOSTILE {
        let r = call(tool, args);
        let refused = r.error && (r.text.starts_with("Refused: ") || r.text.starts_with("ERROR: "));
        kassert!(r.rpc_error == Some(-32602) || refused, "{tool} {args}: {:?} {}", r.rpc_error, r.text);
    }
    Ok(())
}

/// pci_rescan over MCP on an unchanged bus: nothing added, gone or held.
fn rescan() -> TestResult {
    let t = ok("pci_rescan", "{}")?;
    kassert!(t.starts_with("rescan added=0 gone=0 held=0 "), "{t}");
    Ok(())
}

/// QEMU's pci-testdev (1b36:0005): a configuration write at widths 1 and 4 read back (interrupt line, the
/// cache line size dword) and put back; its BAR register refused; bus master switched off, then refused on;
/// memory decoding off makes every BAR access refused, then back on; then its MMIO test protocol through mmio_write/mmio_read: writing
/// test number 0 to offset 0 selects the "no-eventfd" MMIO test, whose header (read byte by byte) names the
/// offset to hit and counts hits; one mmio_write there counts once.
fn testdev() -> TestResult {
    let Some(f) = pci::devices().into_iter().find(|f| f.vendor == 0x1B36 && f.device == 0x0005) else {
        return ktest::skip("no pci-testdev (host/tests/pc/40-raw.sh adds one)");
    };
    let at = loc(f.loc());
    let read = |off: u64, w: u8| -> Result<u64, String> {
        values(&ok("pci_read", &format!(r#"{{"function":"{at}","offset":{off},"width":{w}}}"#))?).first().copied().ok_or_else(|| String::from("no value"))
    };
    let write = |off: u64, w: u8, v: u64| ok("pci_write", &format!(r#"{{"function":"{at}","offset":{off},"width":{w},"value":{v}}}"#));
    let (line, pin) = (read(0x3C, 1)?, read(0x3D, 1)?);
    let t = write(0x3C, 1, 0x5A)?;
    kassert!(t.contains("reads 5a now"), "{t}");
    kassert!(read(0x3D, 1)? == pin, "a byte write changed the pin next to the line");
    write(0x3C, 1, line)?;
    kassert!(read(0x3C, 1)? == line, "interrupt line not put back");
    let cls = read(0x0C, 4)?;
    write(0x0C, 4, (cls & !0xFF) | 0x10)?;
    kassert!(read(0x0C, 1)? == 0x10, "cache line size");
    write(0x0C, 4, cls & 0xFFFF)?;

    let bar = (0..6u8).find(|&b| matches!(f.bar(b), Some(pci::Bar::Mem { .. }))).ok_or("pci-testdev without a memory BAR")?;
    let m = |args: &str| format!(r#"{{"function":"{at}","bar":{bar},{args}}}"#);
    // Its BAR stays where the scan found it; bus master never goes on through a raw write.
    let bar_reg = 0x10 + 4 * bar as u64;
    let bar_now = read(bar_reg, 4)?;
    refused("pci_write", &format!(r#"{{"function":"{at}","offset":{bar_reg},"value":{}}}"#, bar_now ^ 0x10_0000), "BARs, the ROM BAR")?;
    kassert!(read(bar_reg, 4)? == bar_now, "BAR moved");
    // Bus master off is allowed, back on is not (the device has no driver and does no DMA; it stays off).
    if read(4, 2)? & 4 != 0 {
        write(4, 2, read(4, 2)? & !4)?;
    }
    let cmd = read(4, 2)?;
    kassert!(cmd & 4 == 0, "bus master still on: {cmd:#x}");
    refused("pci_write", &format!(r#"{{"function":"{at}","offset":4,"width":2,"value":{}}}"#, cmd | 4), "bus master")?;
    kassert!(read(4, 2)? == cmd, "command changed");
    // Memory decoding off: every BAR access refused (the address is no longer the function's); back on.
    kassert!(cmd & 2 != 0, "pci-testdev does not decode memory: command {cmd:#x}");
    write(4, 2, cmd & !2)?;
    let r = (refused("mmio_read", &m(r#""offset":0"#), "does not decode memory"), refused("mmio_write", &m(r#""offset":0,"width":1,"value":0"#), "does not decode memory"));
    write(4, 2, cmd)?;
    kassert!(read(4, 2)? == cmd, "command not put back");
    r.0?;
    r.1?;
    ok("mmio_write", &m(r#""offset":0,"width":1,"value":0"#))?;
    let hdr = values(&ok("mmio_read", &m(r#""offset":0,"width":1,"count":16"#))?);
    kassert!(hdr.len() == 16, "header {hdr:?}");
    let u32_at = |o: usize| hdr[o] | hdr[o + 1] << 8 | hdr[o + 2] << 16 | hdr[o + 3] << 24;
    let (width, offset, count) = (hdr[1], u32_at(4), u32_at(12));
    kassert!(matches!(width, 1 | 2 | 4) && offset != 0, "test header: width {width} offset {offset:#x} ({hdr:x?})");
    ok("mmio_write", &m(&format!(r#""offset":{offset},"width":{width},"value":{}"#, hdr[8])))?;
    let after = values(&ok("mmio_read", &m(r#""offset":12,"width":1,"count":4"#))?);
    let now = after.iter().enumerate().fold(0, |a, (i, b)| a | b << (8 * i));
    kassert!(now == count + 1, "hit counter {count} -> {now}");
    let t = ok("mmio_write", &m(&format!(r#""offset":{offset},"width":{width},"value":{},"read_back":true"#, hdr[8])))?;
    kassert!(t.contains("reads "), "{t}");
    // Past the BAR and unaligned: bad arguments, not accesses.
    let size = f.bar(bar).map_or(0, |b| b.size());
    kassert!(call("mmio_read", &m(&format!(r#""offset":{size}"#))).rpc_error == Some(-32602), "past the BAR");
    kassert!(call("mmio_write", &m(r#""offset":2,"width":4,"value":0"#)).rpc_error == Some(-32602), "unaligned");
    klog::event!(Info, "pci", "ktest.testdev", function = at.as_str(), bar = bar, test_offset = klog::Hex(offset), hits = now.to_string());
    Ok(())
}
