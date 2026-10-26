//! PC-5: ASPM L1.2 off for one PCIe link, over ECAM.
//!
//! The I226-V loses packets when its link enters L1.2 (spec/net.md §2.1; Linux igc calls
//! `pci_disable_link_state(pdev, PCIE_LINK_STATE_L1_2)` for every I226). V1 could not reach the extended
//! configuration space and switched ASPM L0s and L1 off in Link Control instead (`igc.asm:104-123`). V2 clears
//! only the L1.2 enables in the **L1 PM Substates** extended capability (PCIe 6.0 §7.8.3) of both ends of the
//! link: L0s, L1 and L1.1 stay as the firmware set them.
//!
//! Rules (PCIe 6.0 §5.5.4, Linux `pcie_config_aspm_l1ss`): L1SS control lives in function 0 of a multi-function
//! device (the other functions' copies are ignored), the enables change only while ASPM L1 is off on the link
//! (L1 off device first then port, back on port first then device, as Linux does around every L1SS change),
//! and when disabling, the downstream end (the device) goes first, then the upstream port. Both ASPM L1.2 and PCI-PM L1.2 are cleared: "L1.2 off" means the link never
//! enters it, whichever mechanism asks (PCI-PM L1.2 only matters in D1-D3, so it costs nothing in D0).
//! Without ECAM (0xCF8: 256 bytes) the capability is out of reach; then ASPM L1 itself goes off in Link Control
//! of every function of the device and of the port (V1's way, narrowed to L1; L1.2 is a substate of L1).
//!
//! `l12_off` is pure over `ConfigAccess` (host tests on the fake space); `PciFunction::aspm_l12_off` is the
//! kernel's call with the lock, the record and the events, and `enable_device` runs it for the IDs in
//! `L12_OFF` (the I226 family) so no driver has to remember it.

use crate::access::{ConfigAccess, Loc, EXT_SPACE};
use crate::caps::ext;
use crate::node::Node;
use alloc::vec::Vec;

/// L1 PM Substates Control 1, relative to the capability.
pub const L1SS_CTL1: u16 = 0x08;
pub const PCIPM_L1_2: u32 = 1 << 0;
pub const PCIPM_L1_1: u32 = 1 << 1;
pub const ASPM_L1_2: u32 = 1 << 2;
pub const ASPM_L1_1: u32 = 1 << 3;
/// The bits this helper clears.
pub const L1_2: u32 = PCIPM_L1_2 | ASPM_L1_2;
/// PCIe Link Control, relative to the PCIe capability; bit 1 = ASPM L1 entry, bit 5 = Retrain (reads 0).
const LINK_CTL: u16 = 0x10;
const ASPM_L1: u32 = 1 << 1;
const LINK_RETRAIN: u32 = 1 << 5;

/// Functions whose link must never enter L1.2 (Linux `igc_is_device_id_i226`, IDs from `igc_hw.h`): I226-LM
/// 0x125B, -V 0x125C, -IT 0x125D, blank NVM 0x125F, -LMVP 0x5503, -K 0x3102. `enable_device` switches L1.2
/// off for them.
pub const L12_OFF: &[(u16, u16)] = &[(0x8086, 0x125B), (0x8086, 0x125C), (0x8086, 0x125D), (0x8086, 0x125F), (0x8086, 0x5503), (0x8086, 0x3102)];

pub fn needs_l12_off(vendor: u16, device: u16) -> bool {
    L12_OFF.contains(&(vendor, device))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// Both ends have L1 PM Substates: their L1.2 enables are clear (device first, then the port).
    L1ss,
    /// The extended configuration space is out of reach (no ECAM): ASPM L1 is off in Link Control of the
    /// device's functions and the port instead (V1's way, costs a little power).
    AspmL1Off,
    /// An end has no L1 PM Substates capability: the link cannot enter L1.2, nothing to do.
    NotSupported,
}

impl Method {
    pub fn name(self) -> &'static str {
        match self {
            Method::L1ss => "l1ss",
            Method::AspmL1Off => "aspm_l1_off",
            Method::NotSupported => "no_l1ss",
        }
    }
}

/// One register the helper looked at: L1SS Control 1 (method `L1ss`) or the Link Control dword
/// (`AspmL1Off`), before and after.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct End {
    pub at: Loc,
    pub offset: u16,
    pub before: u32,
    pub after: u32,
}

impl End {
    pub fn changed(&self) -> bool {
        self.before != self.after
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct L12Report {
    pub method: Method,
    /// Function 0 of the device (the end that carries the link's L1SS control).
    pub device: Loc,
    /// The root or downstream port above it.
    pub port: Loc,
    /// In write order: the device's side first, the port last.
    pub ends: Vec<End>,
    /// Read back after the writes: no end still has the bits on.
    pub verified: bool,
    /// Method `L1ss`: the Link Controls whose ASPM L1 was off while the L1SS enables changed (device's
    /// functions first, then the port; `before` is the value put back afterwards). Empty when ASPM L1 was off
    /// already or nothing needed a change.
    pub held: Vec<End>,
}

impl L12Report {
    pub fn changed(&self) -> bool {
        self.ends.iter().any(End::changed)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AspmError {
    NotScanned,
    /// R8d: the function (or its port) stays untouched until this key is on.
    Untouched(&'static str),
    /// R8a: mass storage is written only for the NVMe driver.
    Storage,
    /// Not a PCIe function.
    NotPcie,
    /// No PCIe root or downstream port above it (an integrated endpoint has no link), or the function is
    /// itself such a port (the helper is called for the device below it).
    NoLink,
    /// Reads all ones: the function is gone.
    Gone(Loc),
    /// Another enable/restore held `pci.enable` too long (kernel only).
    Busy,
}

impl core::fmt::Display for AspmError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            AspmError::NotScanned => write!(f, "function not in the scanned tree"),
            AspmError::Untouched(k) => write!(f, "untouched {} (R8d)", crate::registry::Until(k)),
            AspmError::Storage => write!(f, "mass storage is written only for the NVMe driver (R8a)"),
            AspmError::NotPcie => write!(f, "not a PCIe function"),
            AspmError::NoLink => write!(f, "no PCIe link with a root or downstream port above it (call it for the device, not the port)"),
            AspmError::Gone(at) => write!(f, "{at} does not answer"),
            AspmError::Busy => write!(f, "another enable or restore did not finish in time"),
        }
    }
}

/// Clears the L1.2 enables of the link above `nodes[idx]` (see the module text). `may_storage`: the caller is
/// the NVMe driver (R8a). Writes only where a bit is on; a second call writes nothing.
pub fn l12_off(c: &dyn ConfigAccess, nodes: &[Node], idx: usize, may_storage: bool) -> Result<L12Report, AspmError> {
    let n = nodes.get(idx).ok_or(AspmError::NotScanned)?;
    if let Some(k) = n.untouched {
        return Err(AspmError::Untouched(k));
    }
    // A port is the upper end of the links below it: the helper is called for the device below.
    if n.pcie.is_some_and(|p| p.is_root_port() || p.is_downstream_port()) {
        return Err(AspmError::NoLink);
    }
    let fn0 = nodes
        .iter()
        .position(|x| x.loc.segment == n.loc.segment && x.loc.bus == n.loc.bus && x.loc.dev == n.loc.dev && x.loc.func == 0)
        .unwrap_or(idx);
    // Every function of the device: L1SS is fn0's, but the fallback writes each one's Link Control.
    let funcs: Vec<usize> = (0..nodes.len())
        .filter(|&i| {
            let x = &nodes[i].loc;
            x.segment == n.loc.segment && x.bus == n.loc.bus && x.dev == n.loc.dev
        })
        .collect();
    for &i in &funcs {
        if let Some(k) = nodes[i].untouched {
            return Err(AspmError::Untouched(k));
        }
        if nodes[i].is_storage() && !may_storage {
            return Err(AspmError::Storage);
        }
    }
    let dev = &nodes[fn0];
    if n.pcie.is_none() || dev.pcie.is_none() {
        return Err(AspmError::NotPcie);
    }
    let port = dev.parent.and_then(|p| nodes.get(p)).filter(|p| p.pcie.is_some_and(|x| x.is_root_port() || x.is_downstream_port())).ok_or(AspmError::NoLink)?;
    if let Some(k) = port.untouched {
        return Err(AspmError::Untouched(k));
    }
    let report = |method, ends: Vec<End>, verified| L12Report { method, device: dev.loc, port: port.loc, ends, verified, held: Vec::new() };
    let ext_reach = |x: &Node| c.space_size(x.loc.segment, x.loc.bus) >= EXT_SPACE && x.space >= EXT_SPACE;
    if !ext_reach(dev) || !ext_reach(port) {
        let mut ends = Vec::new();
        let order = funcs.iter().map(|&i| &nodes[i]).filter(|x| x.pcie.is_some()).chain(core::iter::once(port));
        for x in order {
            let Some(cap) = x.cap(crate::caps::id::PCIE) else { continue };
            ends.push(clear(c, x.loc, cap.offset as u16 + LINK_CTL, ASPM_L1, true)?);
        }
        let verified = ends.iter().all(|e| e.after & ASPM_L1 == 0);
        return Ok(report(Method::AspmL1Off, ends, verified));
    }
    let (Some(d), Some(p)) = (dev.ext_cap(ext::L1SS), port.ext_cap(ext::L1SS)) else {
        return Ok(report(Method::NotSupported, Vec::new(), true));
    };
    let (d_ctl, p_ctl) = (d.offset + L1SS_CTL1, p.offset + L1SS_CTL1);
    let pending = |at: Loc, off: u16| {
        let v = c.read32(at, off);
        v != u32::MAX && v & L1_2 != 0
    };
    // PCIe 6.0 §5.5.4 (Linux `pcie_config_aspm_link`): the L1SS enables change only while ASPM L1 is off on
    // the link. ASPM L1 goes off in every function of the device first, then in the port; back on in the port
    // first, then the device (§7.5.3.7), with the Link Control values as they were. Nothing of this when no
    // L1.2 bit is on (a second call writes nothing).
    let mut held = Vec::new();
    if pending(dev.loc, d_ctl) || pending(port.loc, p_ctl) {
        let order = funcs.iter().map(|&i| &nodes[i]).filter(|x| x.pcie.is_some()).chain(core::iter::once(port));
        for x in order {
            let Some(cap) = x.cap(crate::caps::id::PCIE) else { continue };
            let off = cap.offset as u16 + LINK_CTL;
            match clear(c, x.loc, off, ASPM_L1, true) {
                Ok(e) if e.changed() => held.push(e),
                Ok(_) => {}
                Err(e) => {
                    resume(c, &held);
                    return Err(e);
                }
            }
        }
    }
    let ends = clear(c, dev.loc, d_ctl, L1_2, false).and_then(|a| Ok(alloc::vec![a, clear(c, port.loc, p_ctl, L1_2, false)?]));
    resume(c, &held);
    let ends = ends?;
    let verified = ends.iter().all(|e| e.after & L1_2 == 0);
    let mut r = report(Method::L1ss, ends, verified);
    r.held = held;
    Ok(r)
}

/// Puts the Link Control values `held` saved back, in reverse order (the port first, then the device's
/// functions). Status half and Retrain written 0; nothing written for an end that is gone.
fn resume(c: &dyn ConfigAccess, held: &[End]) {
    for e in held.iter().rev() {
        c.modify32(e.at, e.offset, &mut |d| (d != u32::MAX).then_some(e.before & 0xFFFF & !LINK_RETRAIN));
    }
}

/// Clears `bits` in the dword at `off` by read-modify-write (nothing written when they are clear) and reads
/// it back. `link_ctl`: the dword is Link Control + Link Status: the status half (write-1-to-clear) and
/// Retrain Link are written 0.
fn clear(c: &dyn ConfigAccess, at: Loc, off: u16, bits: u32, link_ctl: bool) -> Result<End, AspmError> {
    let before = c.modify32(at, off, &mut |d| {
        if d == u32::MAX || d & bits == 0 {
            return None;
        }
        Some(if link_ctl { d & 0xFFFF & !bits & !LINK_RETRAIN } else { d & !bits })
    });
    let after = c.read32(at, off);
    if before == u32::MAX || after == u32::MAX {
        return Err(AspmError::Gone(at));
    }
    // Link Status (the upper half) moves on its own: only the control half is reported.
    let keep = if link_ctl { 0xFFFF } else { u32::MAX };
    Ok(End { at, offset: off, before: before & keep, after: after & keep })
}
