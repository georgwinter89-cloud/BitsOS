//! PC-owned file inside `mcp` (CONTRACTS 6a, MC-2 change request 5: `mcp` depends on `pci`, so the PCI tools
//! cannot be registered in `pci` itself). The raw tools of `spec/net.md` §5.2 that replace V1's `run_asm`
//! pokes: `pci_read`/`pci_write` (configuration space over ECAM), `mmio_read`/`mmio_write` (inside a memory
//! BAR), and `pci_rescan` (PC-4 open point 3). Every refusal is decided by `pci::raw` before any access
//! (class 01 never written or mapped, untouched functions only read in their header, the Radeon's BAR5 index
//! pairs never touched); the writes also pass `policy::check` (B3) as a device access.

use super::{bad, number, opt_bool, opt_str, text};
use crate::{Json, ToolContext, ToolError, ToolResult, mcp_tool};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use pci::raw::{self, Refusal};
use pci::raw_kernel::{self, RawError};

mod ktests;

fn function(args: &Json) -> Result<pci::Loc, ToolError> {
    let s = opt_str(args, "function")?.ok_or_else(|| bad("function"))?;
    raw::parse_loc(s).ok_or_else(|| bad("function"))
}

fn width(args: &Json, allowed: &[u8]) -> Result<u8, ToolError> {
    let w = number(args, "width")?.unwrap_or(4);
    u8::try_from(w).ok().filter(|w| allowed.contains(w)).ok_or_else(|| bad("width"))
}

fn count(args: &Json) -> Result<u64, ToolError> {
    match number(args, "count")?.unwrap_or(1) {
        0 => Err(bad("count")),
        n => Ok(n),
    }
}

/// `value` must fit in `width` bytes.
fn value(args: &Json, width: u8) -> Result<u64, ToolError> {
    let v = number(args, "value")?.ok_or_else(|| bad("value"))?;
    if width < 8 && v >> (8 * width as u32) != 0 {
        return Err(bad("value"));
    }
    Ok(v)
}

fn offset(args: &Json) -> Result<u64, ToolError> {
    number(args, "offset")?.ok_or_else(|| bad("offset"))
}

/// A rule refusal answers `Refused: ...`; an impossible range or width is a bad argument.
fn error(e: RawError) -> ToolError {
    match e {
        RawError::Refused(r) if r.is_rule() => ToolError::Refused(format!("{r}")),
        RawError::Refused(Refusal::NotFound) => ToolError::BadArgument(String::from("function: not in the scanned PCI tree (hw_info section pci lists them)")),
        RawError::Refused(r) => ToolError::BadArgument(format!("{r}")),
        RawError::Map(_) => ToolError::Failed(format!("{e}")),
    }
}

/// B3: a write is a device access by the caller (the kernel's own MCP clients pass; PM-1 decides later).
fn policy(ctx: &ToolContext, at: pci::Loc) -> Result<(), ToolError> {
    let class = match pci::function(at) {
        Some(f) if f.class >> 16 == 0x01 => policy::DeviceClass::Disk,
        Some(f) if f.class >> 16 == 0x03 => policy::DeviceClass::Gpu,
        _ => policy::DeviceClass::Other,
    };
    let name = format!("pci {at}");
    policy::check(&ctx.caller, &policy::Access::Device { class, name: &name }).map_err(|d| ToolError::Refused(d.reason))
}

fn header(at: pci::Loc) -> String {
    match pci::function(at) {
        Some(f) => format!("{at} [{:04x}:{:04x}] class {:#08x}", f.vendor, f.device, f.class),
        None => format!("{at}"),
    }
}

/// Values in rows of 16 bytes: `0x0040: 00100406 ...`.
fn rows(start: u64, width: u8, values: &[u64]) -> Vec<String> {
    let per = (16 / width as usize).max(1);
    values
        .chunks(per)
        .enumerate()
        .map(|(i, row)| {
            let mut s = format!("{:#06x}:", start + (i * per) as u64 * width as u64);
            for v in row {
                s.push_str(&format!(" {:0w$x}", v, w = 2 * width as usize));
            }
            s
        })
        .collect()
}

fn pci_read(_ctx: &ToolContext, args: &Json) -> ToolResult {
    let at = function(args)?;
    let (off, width, count) = (offset(args)?, width(args, &[1, 2, 4])?, count(args)?);
    let r = raw_kernel::config_read(at, off, width, count).map_err(error)?;
    let mut lines = alloc::vec![format!("pci {} offset {off:#x} width {width} count {count} (space {} bytes)", header(at), r.space)];
    if let Some(k) = r.untouched {
        lines.push(format!("untouched {}: header reads only", pci::registry::Until(k)));
    }
    let vals: Vec<u64> = r.values.iter().map(|&v| v as u64).collect();
    lines.extend(rows(off, width, &vals));
    text(lines)
}

fn pci_write(ctx: &ToolContext, args: &Json) -> ToolResult {
    let at = function(args)?;
    let (off, width) = (offset(args)?, width(args, &[1, 2, 4])?);
    let v = value(args, width)?;
    policy(ctx, at)?;
    let w = raw_kernel::config_write(at, off, width, v as u32).map_err(error)?;
    let d = 2 * width as usize;
    text(alloc::vec![format!(
        "pci {} offset {off:#x} width {width}: before {:0d$x}, wrote {:0d$x}, reads {:0d$x} now",
        header(at),
        w.before,
        w.value,
        w.after
    )])
}

fn bar(args: &Json) -> Result<u8, ToolError> {
    let b = number(args, "bar")?.ok_or_else(|| bad("bar"))?;
    u8::try_from(b).ok().filter(|&b| b <= 5).ok_or_else(|| bad("bar"))
}

fn mmio_read(_ctx: &ToolContext, args: &Json) -> ToolResult {
    let at = function(args)?;
    let (b, off, width, count) = (bar(args)?, offset(args)?, width(args, &[1, 2, 4, 8])?, count(args)?);
    let r = raw_kernel::mmio_read(at, b, off, width, count).map_err(error)?;
    let mut lines = alloc::vec![format!(
        "mmio {} bar{b} (phys {:#x}, {:#x} bytes) offset {off:#x} width {width} count {count}",
        header(at),
        r.span.bar_phys,
        r.span.bar_size
    )];
    lines.extend(rows(off, width, &r.values));
    text(lines)
}

fn mmio_write(ctx: &ToolContext, args: &Json) -> ToolResult {
    let at = function(args)?;
    let (b, off, width) = (bar(args)?, offset(args)?, width(args, &[1, 2, 4, 8])?);
    let v = value(args, width)?;
    let read_back = opt_bool(args, "read_back")?.unwrap_or(false);
    policy(ctx, at)?;
    let w = raw_kernel::mmio_write(at, b, off, width, v, read_back).map_err(error)?;
    let d = 2 * width as usize;
    let after = match w.after {
        Some(a) => format!("reads {a:0d$x} now"),
        None => String::from("not read back (read_back:true reads it; a read can have side effects)"),
    };
    text(alloc::vec![format!("mmio {} bar{b} offset {off:#x} (phys {:#x}) width {width}: wrote {v:0d$x}, {after}", header(at), w.span.phys())])
}

fn pci_rescan(ctx: &ToolContext, _args: &Json) -> ToolResult {
    policy::check(&ctx.caller, &policy::Access::Device { class: policy::DeviceClass::Other, name: "pci rescan" }).map_err(|d| ToolError::Refused(d.reason))?;
    let r = pci::rescan().map_err(|e| ToolError::Failed(format!("{e:?}")))?;
    let list = |v: &[pci::Loc]| v.iter().map(|l| format!("{l}")).collect::<Vec<_>>().join(" ");
    text(alloc::vec![
        format!("rescan added={} gone={} held={} kept={} sized={} bound={}", r.added.len(), r.gone.len(), r.held.len(), r.kept, r.sized, r.bound),
        format!("added: {}", list(&r.added)),
        format!("gone: {}", list(&r.gone)),
        format!("held (not ready): {}", list(&r.held)),
    ])
}

mcp_tool!(
    name: "pci_read",
    owner: "PC",
    safety: Read,
    description: "Reads PCI configuration space of one function (ECAM: 4096 bytes for PCIe, else 256): count values of width 1, 2 or 4 bytes (default 4) from offset, at most 65536 bytes. A function left untouched by its key (the WLAN until wlan=on) is read only in its 64-byte header.",
    schema: r#"{"type":"object","properties":{"function":{"type":"string","description":"bus:dev.func or seg:bus:dev.func in hex, e.g. \"03:00.0\""},"offset":{"type":["integer","string"]},"width":{"type":["integer","string"]},"count":{"type":["integer","string"]}},"required":["function","offset"]}"#,
    handler: pci_read,
);
mcp_tool!(
    name: "pci_write",
    owner: "PC",
    safety: Change,
    description: "Writes one value (width 1, 2 or 4 bytes, default 4; 1 and 2 need ECAM) to PCI configuration space at offset and answers the value before and after. Refused: mass storage (class 01) and bridges with mass storage below them (R8a), functions left untouched by their key (R8d); on every function the BARs, ROM BAR, bridge bus numbers and windows, SR-IOV, Resizable BAR, the MSI and MSI-X capabilities, and switching bus master on. Status registers are write-1-to-clear: a 1 there clears that bit.",
    schema: r#"{"type":"object","properties":{"function":{"type":"string"},"offset":{"type":["integer","string"]},"value":{"type":["integer","string"]},"width":{"type":["integer","string"]}},"required":["function","offset","value"]}"#,
    handler: pci_write,
);
mcp_tool!(
    name: "mmio_read",
    owner: "PC",
    safety: Read,
    description: "Reads count values of width 1, 2, 4 or 8 bytes (default 4) at offset inside memory BAR bar (0-5) of one function, uncached, at most 65536 bytes. Only inside an assigned memory BAR; refused for mass storage (class 01), untouched functions, the Radeon's BAR5 index/data pairs, and while the function does not decode memory, is not in D0 or a bridge above does not forward the address.",
    schema: r#"{"type":"object","properties":{"function":{"type":"string"},"bar":{"type":["integer","string"]},"offset":{"type":["integer","string"]},"width":{"type":["integer","string"]},"count":{"type":["integer","string"]}},"required":["function","bar","offset"]}"#,
    handler: mmio_read,
);
mcp_tool!(
    name: "mmio_write",
    owner: "PC",
    safety: Change,
    description: "Writes one value of width 1, 2, 4 or 8 bytes (default 4) at offset inside memory BAR bar (0-5) of one function; read_back:true reads the register again (off by default: reads can have side effects). Refused: mass storage, untouched functions, the MSI-X table and PBA, and the index/data register pairs in the Radeon's BAR5 (only their owner, under its lock); also while the function does not decode memory, is not in D0 or a bridge above does not forward the address.",
    schema: r#"{"type":"object","properties":{"function":{"type":"string"},"bar":{"type":["integer","string"]},"offset":{"type":["integer","string"]},"value":{"type":["integer","string"]},"width":{"type":["integer","string"]},"read_back":{"type":"boolean"}},"required":["function","bar","offset","value"]}"#,
    handler: mmio_write,
);
mcp_tool!(
    name: "pci_rescan",
    owner: "PC",
    safety: Change,
    description: "Walks every PCI bus again: new functions are sized and offered to the drivers, gone ones lose their driver; functions found again keep their BARs and driver (never sized again).",
    schema: r#"{"type":"object","properties":{}}"#,
    handler: pci_rescan,
);

/// Malformed and hostile arguments for these tools: each must be answered with invalid params or a refusal,
/// never a panic and never an access (host test below, ktest `pc.tool.hostile_args`).
pub(crate) const HOSTILE: &[(&str, &str)] = &[
    ("pci_read", "{}"),
    ("pci_read", r#"{"function":"00:00.0"}"#),
    ("pci_read", r#"{"function":7,"offset":0}"#),
    ("pci_read", r#"{"function":"zz","offset":0}"#),
    ("pci_read", r#"{"function":"00:20.0","offset":0}"#),
    ("pci_read", r#"{"function":"00:00.8","offset":0}"#),
    ("pci_read", r#"{"function":"100:00.0","offset":0}"#),
    ("pci_read", r#"{"function":"1:2:3:4","offset":0}"#),
    ("pci_read", r#"{"function":"00:00.0","offset":-4}"#),
    ("pci_read", r#"{"function":"00:00.0","offset":"0xFFFFFFFFFFFFFFFF"}"#),
    ("pci_read", r#"{"function":"00:00.0","offset":1,"width":4}"#),
    ("pci_read", r#"{"function":"00:00.0","offset":0,"width":3}"#),
    ("pci_read", r#"{"function":"00:00.0","offset":0,"width":8}"#),
    ("pci_read", r#"{"function":"00:00.0","offset":0,"count":0}"#),
    ("pci_read", r#"{"function":"00:00.0","offset":0,"count":18446744073709551615}"#),
    ("pci_read", r#"{"function":"00:00.0","offset":4096}"#),
    ("pci_read", r#"{"function":"00:00.0","offset":0,"count":1025}"#),
    ("pci_write", r#"{"function":"00:00.0","offset":0}"#),
    ("pci_write", r#"{"function":"00:00.0","offset":60,"value":256,"width":1}"#),
    ("pci_write", r#"{"function":"00:00.0","offset":0,"value":-1}"#),
    ("pci_write", r#"{"function":"00:00.0","offset":0,"value":"0x1_0000_0000"}"#),
    ("pci_write", r#"{"function":"00:00.0","offset":2,"value":0,"width":4}"#),
    ("mmio_read", r#"{"function":"00:00.0","offset":0}"#),
    ("mmio_read", r#"{"function":"00:00.0","bar":6,"offset":0}"#),
    ("mmio_read", r#"{"function":"00:00.0","bar":-1,"offset":0}"#),
    ("mmio_read", r#"{"function":"00:00.0","bar":0,"offset":0,"width":16}"#),
    ("mmio_read", r#"{"function":"00:00.0","bar":0,"offset":3,"width":2}"#),
    ("mmio_read", r#"{"function":"00:00.0","bar":0,"offset":0,"count":0}"#),
    ("mmio_write", r#"{"function":"00:00.0","bar":0,"offset":0}"#),
    ("mmio_write", r#"{"function":"00:00.0","bar":0,"offset":0,"value":65536,"width":2}"#),
    ("mmio_write", r#"{"function":"00:00.0","bar":0,"offset":0,"value":0,"read_back":"yes"}"#),
    ("mmio_write", r#"{"function":"00:00.0","bar":"x","offset":0,"value":0}"#),
];

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::{Safety, TOOLS, rpc};
    use std::string::ToString;

    fn call(name: &str, args: &str) -> Json {
        let body = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#);
        Json::parse(&rpc::handle(body.as_bytes(), 1_000_000_000, TOOLS.entries()).body).unwrap()
    }
    fn failed(j: &Json) -> bool {
        j.get("result").and_then(|r| r.get("isError")).and_then(Json::as_bool) == Some(true)
    }
    fn text_of(j: &Json) -> String {
        let r = j.get("result");
        r.and_then(|r| r.get("content")).and_then(Json::as_array).and_then(|c| c.first()).and_then(|c| c.get("text")).and_then(Json::as_str).unwrap_or("").to_string()
    }
    fn code(j: &Json) -> Option<i64> {
        j.get("error").and_then(|e| e.get("code")).and_then(Json::as_i64)
    }

    #[test]
    fn registered_with_owner_and_safety() {
        for (name, safety) in [("pci_read", Safety::Read), ("pci_write", Safety::Change), ("mmio_read", Safety::Read), ("mmio_write", Safety::Change), ("pci_rescan", Safety::Change)] {
            let t = crate::find(name).unwrap_or_else(|| panic!("{name} missing"));
            assert_eq!((t.owner, t.safety), ("PC", safety), "{name}");
            assert!(Json::parse(t.input_schema).is_ok(), "{name}");
        }
    }

    #[test]
    fn hostile_arguments_are_answered_not_panicked_on() {
        // On the host no PCI tree exists: every call ends as a bad argument (-32602) or a refusal/error.
        for (tool, args) in HOSTILE {
            let j = call(tool, args);
            let t = text_of(&j);
            let refused = failed(&j) && (t.starts_with("Refused: ") || t.starts_with("ERROR: "));
            assert!(code(&j) == Some(-32602) || refused, "{tool} {args}: {}", j.to_string());
        }
    }

    #[test]
    fn rows_and_values() {
        assert_eq!(rows(0x40, 4, &[1, 2, 3, 4, 5]), ["0x0040: 00000001 00000002 00000003 00000004", "0x0050: 00000005"]);
        assert_eq!(rows(0, 1, &[0xAB]), ["0x0000: ab"]);
        assert_eq!(rows(0x8, 8, &[u64::MAX]), ["0x0008: ffffffffffffffff"]);
        let a = Json::parse(r#"{"value":255,"big":"0x1_0000_0000","w":"2"}"#).unwrap();
        assert_eq!(value(&a, 1), Ok(255));
        assert_eq!(value(&Json::parse(r#"{"value":256}"#).unwrap(), 1), Err(bad("value")));
        assert_eq!(value(&Json::parse(r#"{"value":"0x1_0000_0000"}"#).unwrap(), 4), Err(bad("value")));
        assert_eq!(value(&Json::parse(r#"{"value":"0xFFFFFFFFFFFFFFFF"}"#).unwrap(), 8), Ok(u64::MAX));
        assert_eq!(width(&Json::parse(r#"{"width":"2"}"#).unwrap(), &[1, 2, 4]), Ok(2));
        assert_eq!(width(&Json::parse(r#"{"width":8}"#).unwrap(), &[1, 2, 4]), Err(bad("width")));
    }

    #[test]
    fn unknown_function_is_a_bad_argument() {
        assert_eq!(code(&call("pci_read", r#"{"function":"00:00.0","offset":0}"#)), Some(-32602));
        assert_eq!(code(&call("mmio_write", r#"{"function":"00:02.0","bar":0,"offset":0,"value":1}"#)), Some(-32602));
    }
}
