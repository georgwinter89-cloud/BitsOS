//! Capability lists: the standard list (pointer at 0x34) and the PCIe extended list (from 0x100, ECAM only).
//! Both lists come from the device and are hostile input: every pointer is range-checked, a pointer seen
//! twice ends the walk (a loop), every length is checked against its space. The visited sets are sized by
//! the architecture (64 dwords of standard space, 960 of extended space), not by a count of things (R1).

use crate::access::{read16, read8, ConfigAccess, Loc, EXT_SPACE, STD_SPACE};
use crate::node::{Bars, Bar, PcieInfo, Problem};
use alloc::vec::Vec;

/// Standard capability IDs (PCI 3.0 / PCIe 6.0 §7.5).
pub mod id {
    pub const PM: u8 = 0x01;
    pub const AGP: u8 = 0x02;
    pub const VPD: u8 = 0x03;
    pub const MSI: u8 = 0x05;
    pub const HT: u8 = 0x08;
    pub const VENDOR: u8 = 0x09;
    pub const SUBSYS: u8 = 0x0D;
    pub const PCIX: u8 = 0x07;
    pub const PCIE: u8 = 0x10;
    pub const MSIX: u8 = 0x11;
    pub const SATA: u8 = 0x12;
    pub const AF: u8 = 0x13;
    pub const EA: u8 = 0x14;
}

/// Extended capability IDs.
pub mod ext {
    pub const AER: u16 = 0x0001;
    pub const VC: u16 = 0x0002;
    pub const DSN: u16 = 0x0003;
    pub const VSEC: u16 = 0x000B;
    pub const ACS: u16 = 0x000D;
    pub const ARI: u16 = 0x000E;
    pub const SRIOV: u16 = 0x0010;
    /// Resizable BAR (Linux `PCI_EXT_CAP_ID_REBAR`).
    pub const REBAR: u16 = 0x0015;
    pub const LTR: u16 = 0x0018;
    pub const SECPCI: u16 = 0x0019;
    pub const L1SS: u16 = 0x001E;
    pub const PTM: u16 = 0x001F;
    pub const DVSEC: u16 = 0x0023;
    /// VF Resizable BAR (Linux `PCI_EXT_CAP_ID_VF_REBAR`).
    pub const VF_REBAR: u16 = 0x0024;
    pub const DLF: u16 = 0x0025;
    pub const PL16: u16 = 0x0026;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapInfo {
    Plain,
    /// Power management: current D state from PMCSR (0 = D0 ... 3 = D3hot).
    Pm { version: u8, state: u8, no_soft_reset: bool },
    Msi { enabled: bool, is64: bool, per_vector_mask: bool, vectors: u8 },
    MsiX { enabled: bool, masked: bool, table_size: u16, table_bir: u8, table_offset: u32, pba_bir: u8, pba_offset: u32 },
    Pcie(PcieInfo),
    Vendor { len: u8 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cap {
    pub offset: u8,
    pub id: u8,
    pub info: CapInfo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtInfo {
    Plain,
    /// Advanced Error Reporting: the status registers as found (diagnosis: UR, completion timeouts).
    Aer { uncorrectable: u32, correctable: u32 },
    /// L1 PM Substates: capabilities and control 1 (bits 0-3: PCI-PM L1.2, L1.1, ASPM L1.2, ASPM L1.1).
    L1ss { cap: u32, ctrl1: u32 },
    /// Precision Time Measurement (the I226 hangs if a PTM cycle crosses its reset, spec/net.md §2.2).
    Ptm { cap: u32, ctrl: u32 },
    Vsec { vsec_id: u16, rev: u8, len: u16 },
    Dvsec { vendor: u16, rev: u8, len: u16, dvsec_id: u16 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtCap {
    pub offset: u16,
    pub id: u16,
    pub version: u8,
    pub info: ExtInfo,
}

pub fn std_name(id: u8) -> &'static str {
    match id {
        id::PM => "pm",
        id::AGP => "agp",
        id::VPD => "vpd",
        id::MSI => "msi",
        id::HT => "hypertransport",
        id::PCIX => "pcix",
        id::VENDOR => "vendor",
        id::SUBSYS => "ssid",
        id::PCIE => "pcie",
        id::MSIX => "msix",
        id::SATA => "sata",
        id::AF => "af",
        id::EA => "ea",
        _ => "cap",
    }
}

pub fn ext_name(id: u16) -> &'static str {
    match id {
        ext::AER => "aer",
        ext::VC => "vc",
        ext::DSN => "dsn",
        ext::VSEC => "vsec",
        ext::ACS => "acs",
        ext::ARI => "ari",
        ext::SRIOV => "sriov",
        ext::LTR => "ltr",
        ext::SECPCI => "secpci",
        ext::L1SS => "l1ss",
        ext::PTM => "ptm",
        ext::DVSEC => "dvsec",
        ext::DLF => "dlf",
        ext::PL16 => "pl16",
        _ => "ext",
    }
}

/// (offset, id) of the standard list. Empty when the status register has no capability list (bit 4).
pub fn walk_std(c: &dyn ConfigAccess, at: Loc, status: u16, problems: &mut Vec<Problem>) -> Vec<(u8, u8)> {
    let mut out = Vec::new();
    if status & 0x10 == 0 {
        return out;
    }
    let mut seen = [false; (STD_SPACE / 4) as usize];
    let mut prev = 0x34u16;
    // The low two bits are reserved and must be masked (PCI 3.0 §6.7).
    let mut p = (read8(c, at, 0x34) & 0xFC) as u16;
    while p != 0 {
        if p < 0x40 {
            problems.push(Problem::CapBadPointer { at: prev, ptr: p });
            break;
        }
        if seen[(p / 4) as usize] {
            problems.push(Problem::CapLoop { at: p });
            break;
        }
        seen[(p / 4) as usize] = true;
        let cap_id = read8(c, at, p);
        if cap_id == 0xFF {
            // Nothing answers any more (Linux stops here too).
            break;
        }
        out.push((p as u8, cap_id));
        prev = p;
        p = (read8(c, at, p + 1) & 0xFC) as u16;
    }
    out
}

/// (offset, id, version) of the extended list; needs the 4 KiB space (ECAM).
pub fn walk_ext(c: &dyn ConfigAccess, at: Loc, space: u16, problems: &mut Vec<Problem>) -> Vec<(u16, u16, u8)> {
    let mut out = Vec::new();
    if space < EXT_SPACE {
        return out;
    }
    let mut seen = [false; ((EXT_SPACE - STD_SPACE) / 4) as usize];
    let mut pos = STD_SPACE;
    loop {
        let h = c.read32(at, pos);
        // 0 = no extended capabilities (or a conventional device on ECAM); all ones = nothing answers.
        if h == 0 || h == u32::MAX {
            break;
        }
        let slot = ((pos - STD_SPACE) / 4) as usize;
        if seen[slot] {
            problems.push(Problem::CapLoop { at: pos });
            break;
        }
        seen[slot] = true;
        out.push((pos, (h & 0xFFFF) as u16, ((h >> 16) & 0xF) as u8));
        let next = ((h >> 20) & 0xFFC) as u16;
        if next == 0 {
            break;
        }
        if next < STD_SPACE {
            problems.push(Problem::CapBadPointer { at: pos, ptr: next });
            break;
        }
        pos = next;
    }
    out
}

/// Length a standard capability needs for what we read of it; `None` if unknown/variable.
fn std_len(c: &dyn ConfigAccess, at: Loc, off: u16, cap_id: u8) -> Option<u16> {
    match cap_id {
        id::PM => Some(8),
        id::MSI => {
            let ctrl = read16(c, at, off + 2);
            Some(match (ctrl & 0x80 != 0, ctrl & 0x100 != 0) {
                (false, false) => 0x0A,
                (true, false) => 0x0E,
                (false, true) => 0x14,
                (true, true) => 0x18,
            })
        }
        id::MSIX => Some(0x0C),
        // Version 1 ends after the link registers we read; version 2 is 0x3C long.
        id::PCIE => Some(if read16(c, at, off + 2) & 0xF >= 2 { 0x3C } else { 0x14 }),
        _ => None,
    }
}

/// Decodes one standard capability. A capability that runs past 0x100 is recorded as `Plain` with a
/// problem; nothing past its space is read.
pub fn decode_std(c: &dyn ConfigAccess, at: Loc, off: u8, cap_id: u8, problems: &mut Vec<Problem>) -> CapInfo {
    let off = off as u16;
    if cap_id == id::VENDOR {
        let len = read8(c, at, off + 2);
        if len < 3 || off + len as u16 > STD_SPACE {
            problems.push(Problem::CapBadLength { id: cap_id as u16, at: off, len: len as u16 });
            return CapInfo::Plain;
        }
        return CapInfo::Vendor { len };
    }
    if let Some(len) = std_len(c, at, off, cap_id)
        && off + len > STD_SPACE
    {
        problems.push(Problem::CapBadLength { id: cap_id as u16, at: off, len });
        // The PCIe v2 tail is optional for us; the v1 part still fits when the cap starts by 0xEC.
        if !(cap_id == id::PCIE && off + 0x14 <= STD_SPACE) {
            return CapInfo::Plain;
        }
    }
    let r16 = |o: u16| read16(c, at, off + o);
    let r32 = |o: u16| c.read32(at, off + o);
    match cap_id {
        id::PM => {
            let pmc = r16(2);
            let csr = r16(4);
            CapInfo::Pm { version: (pmc & 7) as u8, state: (csr & 3) as u8, no_soft_reset: csr & 0x8 != 0 }
        }
        id::MSI => {
            let ctrl = r16(2);
            CapInfo::Msi { enabled: ctrl & 1 != 0, is64: ctrl & 0x80 != 0, per_vector_mask: ctrl & 0x100 != 0, vectors: 1 << ((ctrl >> 1) & 7).min(5) }
        }
        id::MSIX => {
            let ctrl = r16(2);
            let t = r32(4);
            let p = r32(8);
            CapInfo::MsiX {
                enabled: ctrl & 0x8000 != 0,
                masked: ctrl & 0x4000 != 0,
                table_size: (ctrl & 0x7FF) + 1,
                table_bir: (t & 7) as u8,
                table_offset: t & !7,
                pba_bir: (p & 7) as u8,
                pba_offset: p & !7,
            }
        }
        id::PCIE => {
            let caps = r16(2);
            let link_cap = r32(0x0C);
            let link_sta = r16(0x12);
            CapInfo::Pcie(PcieInfo {
                version: (caps & 0xF) as u8,
                port_type: ((caps >> 4) & 0xF) as u8,
                link_speed: (link_sta & 0xF) as u8,
                link_width: ((link_sta >> 4) & 0x3F) as u8,
                max_speed: (link_cap & 0xF) as u8,
                max_width: ((link_cap >> 4) & 0x3F) as u8,
                dev_status: r16(0x0A),
            })
        }
        _ => CapInfo::Plain,
    }
}

/// Decodes one extended capability, bounds-checked against the 4 KiB space.
pub fn decode_ext(c: &dyn ConfigAccess, at: Loc, off: u16, ext_id: u16, problems: &mut Vec<Problem>) -> ExtInfo {
    let need: u16 = match ext_id {
        ext::AER => 0x14,
        ext::L1SS => 0x10,
        ext::PTM => 0x0C,
        ext::VSEC | ext::DVSEC => 0x0C,
        _ => return ExtInfo::Plain,
    };
    if off as u32 + need as u32 > EXT_SPACE as u32 {
        problems.push(Problem::CapBadLength { id: ext_id, at: off, len: need });
        return ExtInfo::Plain;
    }
    let r32 = |o: u16| c.read32(at, off + o);
    match ext_id {
        ext::AER => ExtInfo::Aer { uncorrectable: r32(4), correctable: r32(0x10) },
        ext::L1SS => ExtInfo::L1ss { cap: r32(4), ctrl1: r32(8) },
        ext::PTM => ExtInfo::Ptm { cap: r32(4), ctrl: r32(8) },
        ext::VSEC | ext::DVSEC => {
            let h = r32(4);
            let len = ((h >> 20) & 0xFFF) as u16;
            let min = if ext_id == ext::VSEC { 8 } else { 0x0A };
            if len < min || off as u32 + len as u32 > EXT_SPACE as u32 {
                problems.push(Problem::CapBadLength { id: ext_id, at: off, len });
                return ExtInfo::Plain;
            }
            if ext_id == ext::VSEC {
                ExtInfo::Vsec { vsec_id: h as u16, rev: ((h >> 16) & 0xF) as u8, len }
            } else {
                ExtInfo::Dvsec { vendor: h as u16, rev: ((h >> 16) & 0xF) as u8, len, dvsec_id: r32(8) as u16 }
            }
        }
        _ => ExtInfo::Plain,
    }
}

/// Bytes of configuration space this function really has (Linux `pci_cfg_space_size`): the extended space
/// only for a PCIe function or a PCI-X mode 2 (266/533 MHz) one. A conventional function behind a
/// PCIe-to-PCI bridge may alias or return junk at 0x100, which must not become phantom capabilities.
pub fn effective_space(c: &dyn ConfigAccess, at: Loc, caps: &[(u8, u8)], space: u16) -> u16 {
    if space <= STD_SPACE {
        return space;
    }
    let ext_capable = caps.iter().any(|&(off, cap_id)| {
        cap_id == id::PCIE || (cap_id == id::PCIX && off as u16 + 8 <= STD_SPACE && c.read32(at, off as u16 + 4) & 0xC000_0000 != 0)
    });
    if ext_capable { space } else { STD_SPACE }
}

/// Both lists with their decoded contents, and the function's configuration space size (`effective_space`).
pub fn read_all(c: &dyn ConfigAccess, at: Loc, status: u16, space: u16, problems: &mut Vec<Problem>) -> (Vec<Cap>, Vec<ExtCap>, u16) {
    let list = walk_std(c, at, status, problems);
    let space = effective_space(c, at, &list, space);
    let std: Vec<Cap> = list.into_iter().map(|(offset, cap_id)| Cap { offset, id: cap_id, info: decode_std(c, at, offset, cap_id, problems) }).collect();
    let extl: Vec<ExtCap> = walk_ext(c, at, space, problems)
        .into_iter()
        .map(|(offset, ext_id, version)| ExtCap { offset, id: ext_id, version, info: decode_ext(c, at, offset, ext_id, problems) })
        .collect();
    (std, extl, space)
}

/// The MSI-X table and PBA must lie inside a memory BAR of the function (checked after sizing; a driver
/// maps them through that BAR).
pub fn check_msix(caps: &[Cap], bars: &Bars, problems: &mut Vec<Problem>) {
    for cap in caps {
        let CapInfo::MsiX { table_size, table_bir, table_offset, pba_bir, pba_offset, .. } = cap.info else { continue };
        let table_end = table_offset as u64 + table_size as u64 * 16;
        // PBA: one bit per vector, in qwords.
        let pba_end = pba_offset as u64 + (table_size as u64).div_ceil(64) * 8;
        for (bir, offset, end) in [(table_bir, table_offset, table_end), (pba_bir, pba_offset, pba_end)] {
            let ok = match bars.get(bir as usize).copied().flatten() {
                Some(Bar::Mem { size, .. }) => end <= size,
                _ => false,
            };
            if !ok {
                problems.push(Problem::MsixOutside { bir, offset });
            }
        }
    }
}
