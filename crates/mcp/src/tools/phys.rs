//! `read_phys`: physical RAM, read-only, through the direct map (`spec/net.md` §5.2, V1's `read_memory`). Only
//! ranges the firmware memory map calls RAM are read (the same rule bitsboot used to build the direct map), so
//! no device register (MMIO, a BAR, the LAPIC) and no hole is ever touched; at most `PHYS_MAX` bytes per call.

use super::{bad, number, opt_str};
use crate::{Json, ToolContext, ToolError, ToolOutput, ToolResult, b64, mcp_tool};
use acpi::memmap::{self, MemMap};
use alloc::format;
use alloc::string::String;
use alloc::vec;

/// Bytes per call (a bound on one request, not a count of things): 64 KiB as hex is about 300 KB of answer.
pub const PHYS_MAX: u64 = 64 * 1024;
const DEFAULT_LEN: u64 = 256;

/// Whether `[addr, addr + len)` may be read (pure, host-tested). Err is the refusal text.
pub fn phys_check(map: Option<&MemMap>, addr: u64, len: u64) -> Result<(), String> {
    if len == 0 || len > PHYS_MAX {
        return Err(format!("len must be 1 to {PHYS_MAX}"));
    }
    let end = addr.checked_add(len).ok_or_else(|| String::from("the range wraps around the address space"))?;
    let Some(map) = map.filter(|m| !m.is_empty()) else {
        return Err(String::from("no firmware memory map is known (acpi::init did not run), so nothing counts as RAM"));
    };
    if map.covers(addr, end, memmap::is_ram) {
        return Ok(());
    }
    // Name the first part that is not RAM.
    let mut at = addr;
    while at < end {
        match map.find(at) {
            Some(r) if memmap::is_ram(r.ty) => at = r.end,
            Some(r) => return Err(format!("{at:#x} is not RAM (memory map type {:#x}, {:#x}-{:#x}): MMIO, firmware or reserved memory is never read", r.ty, r.start, r.end)),
            None => return Err(format!("{at:#x} is outside the firmware memory map (a hole or device space)")),
        }
    }
    Err(String::from("the range is not RAM"))
}

/// Classic hex dump: address, 16 bytes, their printable characters.
pub(crate) fn hex_dump(addr: u64, b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 5);
    for (i, row) in b.chunks(16).enumerate() {
        if i > 0 {
            s.push('\n');
        }
        s.push_str(&format!("{:#014x}:", addr + 16 * i as u64));
        for x in row {
            s.push_str(&format!(" {x:02x}"));
        }
        for _ in row.len()..16 {
            s.push_str("   ");
        }
        s.push_str("  |");
        s.extend(row.iter().map(|&c| if (0x20..0x7F).contains(&c) { c as char } else { '.' }));
        s.push('|');
    }
    s
}

fn read_phys(_ctx: &ToolContext, args: &Json) -> ToolResult {
    let addr = number(args, "addr")?.ok_or_else(|| bad("addr"))?;
    let len = number(args, "len")?.unwrap_or(DEFAULT_LEN);
    let base64 = match opt_str(args, "format")? {
        None | Some("hex") => false,
        Some("base64") => true,
        Some(_) => return Err(bad("format")),
    };
    if let Err(why) = phys_check(acpi::live::memory_map(), addr, len) {
        klog::event!(Info, "mcp", "phys.refused", addr = klog::Hex(addr), len = len);
        return Err(ToolError::Refused(why));
    }
    let mut buf = vec![0u8; len as usize];
    // No plain "read RAM" primitive in hal yet (change request in plan/notes/mc-MC-2.md): dma_buf::read is the
    // bounds-checked volatile read through the direct map, and the range was just checked to be RAM.
    if !hal::dma_buf::read(addr, buf.len(), 0, &mut buf) {
        return Err(ToolError::Failed(String::from("the direct-map read refused the range")));
    }
    let body = if base64 {
        let mut s = String::new();
        b64::encode_into(&buf, &mut s);
        s
    } else {
        hex_dump(addr, &buf)
    };
    Ok(vec![ToolOutput::Text(format!("phys {addr:#x} len={len}\n{body}"))])
}

mcp_tool!(
    name: "read_phys",
    owner: "MC",
    safety: Read,
    description: "Reads physical RAM (read-only, through the kernel's direct map): len bytes (1 to 65536, default 256) at addr, as a hex dump or base64. Only what the firmware memory map calls RAM; MMIO, device space, reserved firmware memory and holes are refused.",
    schema: r#"{"type":"object","properties":{"addr":{"type":["integer","string"],"description":"physical address, e.g. 1048576 or \"0x100000\""},"len":{"type":["integer","string"]},"format":{"type":"string","enum":["hex","base64"]}},"required":["addr"]}"#,
    handler: read_phys,
);

#[cfg(test)]
mod tests {
    use super::*;
    use acpi::memmap::Region;
    use bootinfo::memtype;

    fn map() -> MemMap {
        let r = |start, end, ty| Region { start, end, ty };
        MemMap::new(vec![
            r(0, 0x9F000, memtype::CONVENTIONAL),
            r(0x100000, 0x800000, memtype::CONVENTIONAL),
            r(0x800000, 0x900000, memtype::ACPI_NVS),
            r(0x900000, 0xA00000, memmap::RESERVED),
            r(0xFEC00000, 0xFEC01000, memtype::MMIO),
        ])
    }

    #[test]
    fn only_ram_inside_the_bound() {
        let m = map();
        assert_eq!(phys_check(Some(&m), 0x100000, 4096), Ok(()));
        assert_eq!(phys_check(Some(&m), 0x7FF000, 0x2000), Ok(()), "RAM to RAM of another type (ACPI NVS is RAM)");
        assert!(phys_check(Some(&m), 0x9E000, 0x2000).unwrap_err().contains("outside"), "into the legacy hole");
        assert!(phys_check(Some(&m), 0xFEC00000, 4).unwrap_err().contains("not RAM"), "I/O APIC");
        assert!(phys_check(Some(&m), 0x8FFFF0, 0x20).unwrap_err().contains("0x900000 is not RAM"));
        assert!(phys_check(Some(&m), 0xFEE00000, 4).unwrap_err().contains("outside"), "LAPIC: not in the map");
        assert!(phys_check(Some(&m), 0x100000, 0).is_err());
        assert!(phys_check(Some(&m), 0x100000, PHYS_MAX + 1).is_err());
        assert_eq!(phys_check(Some(&m), 0x100000, PHYS_MAX), Ok(()));
        assert!(phys_check(Some(&m), u64::MAX - 3, 8).unwrap_err().contains("wraps"));
        assert!(phys_check(None, 0x100000, 16).unwrap_err().contains("no firmware memory map"));
        assert!(phys_check(Some(&MemMap::default()), 0x100000, 16).is_err());
    }

    #[test]
    fn dump() {
        let d = hex_dump(0x1000, b"BitsOS V2 kernel\x00\x01");
        assert_eq!(d, "0x000000001000: 42 69 74 73 4f 53 20 56 32 20 6b 65 72 6e 65 6c  |BitsOS V2 kernel|\n0x000000001010: 00 01                                            |..|");
    }
}
