//! PC-2: switching a function on for its driver and off again, over any configuration back end (the host
//! tests drive it with the fake space; `device.rs` wraps it for the kernel with the lock, the events and the
//! per-function record).
//!
//! `enable` does what Linux's `pci_enable_device` + `pci_set_master` do, plus the Mini-PC's lessons
//! (spec/usb.md §8, spec/net.md §2.1, spec/core.md §11 trap 1 and 14):
//! 1. Every bridge from the root down, then the function: D1/D2/D3hot -> D0 through PMCSR, with the PCI PM
//!    1.2 recovery time (10 ms after D3hot), one retry (Linux `pci_enable_bridge` enables each bridge, which
//!    powers it up: a bridge in D3hot forwards nothing below it). Whatever comes back reset (No_Soft_Reset
//!    clear) gets its header and PCIe control words (`PcieCtl`: Device/Link/Slot/Root Control, Device and
//!    Link Control 2) written back from a copy read before the wake (configuration space is readable in
//!    D3hot); a function the scan could not size (it was in D3hot) is sized now.
//! 2. On the first enable since boot or since `disable`: MSI and MSI-X enables a previous kernel or the
//!    firmware left on are cleared (a Reload otherwise leaves a live MSI aimed at a dead IDT). Later enables
//!    leave them: by then they are the driver's own vectors (PC-3).
//! 3. Every bridge from the root down to the function: memory/I/O forwarding on where the function needs it
//!    and the bridge has a window, and **bus master on when `dma`**. The firmware leaves bus master off on the
//!    root ports of devices it does not use, and the port then answers the device's DMA with Unsupported
//!    Request (memory `minipc-pcie-busmaster`; Linux `pci_enable_bridge`). Bits are only ever set here.
//! 4. The function: memory/I/O decoding as its BARs need, INTx off, bus master when `dma`.
//!
//! Command and MSI control registers are changed by read-modify-write (`ConfigAccess::modify32`), which the
//! kernel's wrapper makes atomic under `pci.cfg`. Nothing here logs: the caller turns the returned changes
//! into events after the sequence.

use crate::access::{modify_command, read16, vendor_present, ConfigAccess, Loc};
use crate::bars;
use crate::caps::id;
use crate::node::{ancestors, Bar, Bars, Node};
use alloc::vec::Vec;

pub const CMD_IO: u16 = 1 << 0;
pub const CMD_MEM: u16 = 1 << 1;
pub const CMD_MASTER: u16 = 1 << 2;
pub const CMD_INTX_OFF: u16 = 1 << 10;

/// PCI PM 1.2 §5.6.1: D3hot -> D0 needs 10 ms before the function may be accessed, D2 -> D0 200 µs.
pub const D3HOT_DELAY_NS: u64 = 10_000_000;
pub const D2_DELAY_NS: u64 = 200_000;

/// PMCSR bits.
const PM_STATE: u16 = 0x3;
const PM_NO_SOFT_RESET: u16 = 1 << 3;

/// One thing `enable`, `restore` or `disable` changed (or found). Each becomes an event in `device.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// `at` (the function or a bridge above it) left a low power state (`from`: 1 = D1, 2 = D2,
    /// 3 = D3hot). `soft_reset`: it came back reset.
    Woke { at: Loc, from: u8, waited_ns: u64, soft_reset: bool },
    /// Still not in D0 after the recovery time; waited once more.
    WakeRetry { at: Loc, state: u8 },
    /// A configuration dword of `at` written back (after a reset on wake or a link retrain).
    Restored { at: Loc, offset: u16, was: u32, now: u32 },
    MsiCleared,
    MsixCleared,
    /// The function's command register.
    Command { old: u16, new: u16 },
    /// A bridge on the path (bus master and/or forwarding switched on).
    Bridge { at: Loc, old: u16, new: u16 },
    /// An assigned BAR that no window of a bridge above forwards: the device cannot be reached there.
    BarOutsideWindow { index: u8, bridge: Loc, start: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnableError {
    /// R8d: the function stays untouched until this CONFIG key is `on`.
    Untouched(&'static str),
    /// R8a: mass storage is enabled only for the NVMe driver.
    Storage,
    /// The function does not answer (vendor all ones) or answers with another ID.
    Gone,
    /// The function stayed in this D state after the recovery time and one retry.
    PowerStuck { state: u8 },
    /// A bridge on the path stayed in this D state (nothing below it can be reached).
    BridgeAsleep { at: Loc, state: u8 },
    /// Another enable or restore held `pci.enable` for longer than the bound (B4: no unbounded wait), or the
    /// function's driver is being removed (PC-4).
    Busy,
    /// A bridge on the path is untouched (cannot happen with the scan's rule; refused rather than guessed).
    BridgeUntouched(Loc),
    /// The function is not in the scanned tree.
    NotScanned,
}

impl core::fmt::Display for EnableError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            EnableError::Untouched(k) => write!(f, "untouched {} (R8d)", crate::registry::Until(k)),
            EnableError::Storage => write!(f, "mass storage is enabled only for the NVMe driver (R8a)"),
            EnableError::Gone => write!(f, "function does not answer"),
            EnableError::PowerStuck { state } => write!(f, "function stays in D{state}"),
            EnableError::BridgeAsleep { at, state } => write!(f, "bridge {at} on the path stays in D{state}"),
            EnableError::Busy => write!(f, "another enable or restore did not finish in time, or the driver is being removed"),
            EnableError::BridgeUntouched(at) => write!(f, "bridge {at} on the path is untouched"),
            EnableError::NotScanned => write!(f, "function not in the scanned tree"),
        }
    }
}

impl EnableError {
    /// Event name for the log.
    pub fn name(&self) -> &'static str {
        match self {
            EnableError::Untouched(_) | EnableError::Storage => "enable.refused",
            _ => "enable.failed",
        }
    }
}

/// The 64-byte header as read at one moment (Linux `pci_save_state` keeps the same 16 dwords), plus the
/// PCIe control words a reset also loses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Saved {
    pub at: Loc,
    pub header: [u32; 16],
    pub pcie: Option<PcieCtl>,
}

/// The PCIe capability's control words a reset or a link retrain loses (Linux `pci_save_pcie_state` keeps
/// the same set): Device Control (max payload / read request size, relaxed ordering, error reporting) and Link
/// Control (ASPM, RCB, common clock) of every function with a PCIe capability, and since PC-5 the registers
/// Linux keeps besides: Slot Control (ports with a slot), Root Control (root ports and event collectors), and
/// the "2" registers of a capability version 2 or later (Device Control 2: completion timeout, ARI forwarding,
/// LTR, 10-bit tags; Link Control 2: target link speed, de-emphasis). `None` = the register does not exist on
/// this function (Linux `pcie_capability_reg_implemented`); such a register is never read back or written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcieCtl {
    /// Offset of the PCIe capability.
    pub cap: u8,
    pub dev_ctl: u16,
    /// `None` for root complex integrated endpoints and event collectors (no link).
    pub link_ctl: Option<u16>,
    pub slot_ctl: Option<u16>,
    pub root_ctl: Option<u16>,
    pub dev_ctl2: Option<u16>,
    pub link_ctl2: Option<u16>,
}

const PCIE_FLAGS: u16 = 0x02;
const PCIE_DEV_CTL: u16 = 0x08;
const PCIE_LINK_CTL: u16 = 0x10;
const PCIE_SLOT_CTL: u16 = 0x18;
const PCIE_ROOT_CTL: u16 = 0x1C;
const PCIE_DEV_CTL2: u16 = 0x28;
const PCIE_LINK_CTL2: u16 = 0x30;
/// Link Control bit 5 (Retrain Link) always reads 0; never written back as 1.
const LINK_RETRAIN: u16 = 1 << 5;
/// PCIe Capabilities register bit 8: Slot Implemented (a port wired to a slot).
const FLAG_SLOT: u16 = 1 << 8;

/// Which optional PCIe registers a function has, from its PCIe Capabilities register (cap + 2): version in
/// bits 3:0, device/port type in bits 7:4, Slot Implemented in bit 8. Mirrors Linux `pcie_cap_has_lnkctl`,
/// `pcie_cap_has_sltctl`, `pcie_cap_has_rtctl` and the version check of `pcie_capability_reg_implemented`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcieRegs {
    pub link: bool,
    pub slot: bool,
    pub root: bool,
    /// Device Control 2 / Link Control 2 (capability version 2 or later).
    pub v2: bool,
}

impl PcieRegs {
    pub fn from_flags(flags: u16) -> PcieRegs {
        let version = flags & 0xF;
        let ty = (flags >> 4) & 0xF;
        // 0 endpoint, 1 legacy endpoint, 4 root port, 5 upstream, 6 downstream, 7 PCIe-to-PCI, 8 PCI-to-PCIe.
        let link = matches!(ty, 0 | 1 | 4 | 5 | 6 | 7 | 8);
        // Linux `pcie_downstream_port`: root port, downstream port, PCI/PCI-X-to-PCIe bridge (type 8).
        let downstream = matches!(ty, 4 | 6 | 8);
        PcieRegs { link, slot: downstream && flags & FLAG_SLOT != 0, root: matches!(ty, 4 | 10), v2: version > 1 }
    }
}

/// `pcie`: offset of the PCIe capability (0 = none).
pub fn save(c: &dyn ConfigAccess, at: Loc, pcie: u8) -> Saved {
    let mut header = [0u32; 16];
    for (i, d) in header.iter_mut().enumerate() {
        *d = c.read32(at, 4 * i as u16);
    }
    let pcie = (pcie != 0).then(|| {
        let base = pcie as u16;
        let regs = PcieRegs::from_flags(read16(c, at, base + PCIE_FLAGS));
        let r = |on: bool, off: u16| on.then(|| read16(c, at, base + off));
        PcieCtl {
            cap: pcie,
            dev_ctl: read16(c, at, base + PCIE_DEV_CTL),
            link_ctl: r(regs.link, PCIE_LINK_CTL).map(|v| v & !LINK_RETRAIN),
            slot_ctl: r(regs.slot, PCIE_SLOT_CTL),
            root_ctl: r(regs.root, PCIE_ROOT_CTL),
            dev_ctl2: r(regs.v2, PCIE_DEV_CTL2),
            link_ctl2: r(regs.v2 && regs.link, PCIE_LINK_CTL2),
        }
    });
    Saved { at, header, pcie }
}

impl PcieCtl {
    /// (offset inside the capability, saved value, bits never compared or written as 1) in Linux's restore
    /// order (`pci_restore_pcie_state`): Device Control, Link Control, Slot Control, Root Control, Device
    /// Control 2, Link Control 2. Each upper half (a status register, write-1-to-clear) is written 0.
    pub fn words(&self) -> Vec<(u16, u16, u16)> {
        let mut v = alloc::vec![(PCIE_DEV_CTL, self.dev_ctl, 0)];
        for (off, val, ignore) in [
            (PCIE_LINK_CTL, self.link_ctl, LINK_RETRAIN),
            (PCIE_SLOT_CTL, self.slot_ctl, 0),
            (PCIE_ROOT_CTL, self.root_ctl, 0),
            (PCIE_DEV_CTL2, self.dev_ctl2, 0),
            (PCIE_LINK_CTL2, self.link_ctl2, 0),
        ] {
            if let Some(x) = val {
                v.push((off, x & !ignore, ignore));
            }
        }
        v
    }
}

/// Dwords that `restore` writes back, highest first like Linux `pci_restore_config_space`. Not: IDs (0x00),
/// command/status (last, separately), class (0x08), the capability pointer (0x34). BIST is written as 0.
const RESTORE: [usize; 12] = [15, 14, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3];

fn restore_value(i: usize, v: u32) -> u32 {
    if i == 3 { v & 0x00FF_FFFF } else { v }
}

/// Writes back every header dword that differs from `saved` (BARs, cache line, interrupt line; for a bridge
/// also bus numbers and windows) and the PCIe control words, with decoding off while BARs move (Linux does
/// the same for 64-bit BARs), command register last. `keep_decode`: host bridges never stop decoding.
///
/// The command register only gains bits: the decoding `saved` had, and bus master only when `master` (the
/// caller's current intent; a `disable` since the save must not be undone). Bits on now stay on. Returns
/// what was written (no entry for the command when it ends as it was).
pub fn restore(c: &dyn ConfigAccess, saved: &Saved, keep_decode: bool, master: bool) -> Result<Vec<Change>, EnableError> {
    let at = saved.at;
    let id = c.read32(at, 0);
    if id != saved.header[0] {
        return Err(EnableError::Gone);
    }
    let mut out = Vec::new();
    if let Some(p) = saved.pcie {
        // Retrain Link (Link Control bit 5) reads 0 and is never compared or written as 1; in Device Control
        // bit 5 is part of max payload size and compared as it is.
        for (off, want, ignore) in p.words() {
            let o = p.cap as u16 + off;
            let d = c.read32(at, o);
            if d != u32::MAX && (d as u16) & !ignore != want {
                // The status half is write-1-to-clear: written 0.
                c.write32(at, o, want as u32);
                out.push(Change::Restored { at, offset: o, was: d & 0xFFFF, now: want as u32 });
            }
        }
    }
    let now: Vec<u32> = (0..16).map(|i| c.read32(at, 4 * i as u16)).collect();
    let differs: Vec<usize> = RESTORE.iter().copied().filter(|&i| restore_value(i, now[i]) != restore_value(i, saved.header[i])).collect();
    let decode_bits = CMD_IO | CMD_MEM;
    let cmd_saved = saved.header[1] as u16;
    let mut cmd_now = now[1] as u16;
    let mut switched_off = false;
    if !differs.is_empty() && !keep_decode && cmd_now & decode_bits != 0 {
        let old = modify_command(c, at, |cur| (cur != 0xFFFF).then_some(cur & !decode_bits));
        if old == 0xFFFF {
            return Err(EnableError::Gone);
        }
        cmd_now = old;
        switched_off = true;
    }
    for i in differs {
        let v = restore_value(i, saved.header[i]);
        c.write32(at, 4 * i as u16, v);
        out.push(Change::Restored { at, offset: 4 * i as u16, was: now[i], now: v });
    }
    let add = (cmd_saved & decode_bits) | if master { CMD_MASTER } else { 0 };
    let mut before = cmd_now;
    let mut after = cmd_now;
    modify_command(c, at, |cur| {
        if cur == 0xFFFF {
            return None;
        }
        // Bits a concurrent change set meanwhile are kept; the decoding switched off above comes back.
        before = if switched_off { cur | (cmd_now & decode_bits) } else { cur };
        after = before | add;
        (after != cur).then_some(after)
    });
    if after != before {
        out.push(Change::Restored { at, offset: 4, was: before as u32, now: after as u32 });
    }
    Ok(out)
}

/// Brings a function or bridge to D0 (no PM capability: nothing to do). `wait` sleeps the given nanoseconds
/// without any lock held. On a reset during the wake the header and the PCIe control words (`pcie`: the
/// capability offset, 0 = none) are written back from the copy read before it.
pub fn wake(c: &dyn ConfigAccess, at: Loc, pm: u8, pcie: u8, wait: &mut dyn FnMut(u64), out: &mut Vec<Change>) -> Result<(), EnableError> {
    let off = pm as u16 + 4;
    let dword = c.read32(at, off);
    if dword == u32::MAX {
        return Err(EnableError::Gone);
    }
    let csr = dword as u16;
    let from = (csr & PM_STATE) as u8;
    if from == 0 {
        return Ok(());
    }
    // Configuration space answers in D1-D3hot: the header is read before the wake may reset it.
    let before = save(c, at, pcie);
    // Linux `pci_power_up`: the whole PMCSR word 0 (state D0, PME_En off; PME_Status is write-1-to-clear).
    c.write32(at, off, dword & 0xFFFF_0000);
    let delay = match from {
        3 => D3HOT_DELAY_NS,
        2 => D2_DELAY_NS,
        _ => 0,
    };
    let mut waited = 0;
    if delay > 0 {
        wait(delay);
        waited += delay;
    }
    let mut state = (read16(c, at, off) & PM_STATE) as u8;
    if state != 0 {
        out.push(Change::WakeRetry { at, state });
        wait(D3HOT_DELAY_NS);
        waited += D3HOT_DELAY_NS;
        state = (read16(c, at, off) & PM_STATE) as u8;
        if state != 0 {
            return Err(EnableError::PowerStuck { state });
        }
    }
    let soft_reset = from == 3 && csr & PM_NO_SOFT_RESET == 0;
    out.push(Change::Woke { at, from, waited_ns: waited, soft_reset });
    if soft_reset {
        out.extend(restore(c, &before, false, before.header[1] as u16 & CMD_MASTER != 0)?);
    }
    Ok(())
}

/// What `enable` did. `error`: why it stopped (the changes up to there are still listed and must be logged).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub changes: Vec<Change>,
    /// BARs sized during the enable (the scan could not: the function was in D3hot).
    pub sized: Option<Bars>,
    /// The header after the enable (the copy `restore_config` writes back after a link retrain).
    pub saved: Option<Saved>,
    pub error: Option<EnableError>,
}

impl Outcome {
    fn fail(changes: Vec<Change>, e: EnableError) -> Outcome {
        Outcome { changes, sized: None, saved: None, error: Some(e) }
    }
}

/// Memory/I/O decoding a set of BARs needs.
pub fn decode_needed(bars: Option<&Bars>) -> u16 {
    let mut need = 0;
    for b in bars.into_iter().flatten().flatten().filter(|b| b.assigned()) {
        need |= match b {
            Bar::Mem { .. } => CMD_MEM,
            Bar::Io { .. } => CMD_IO,
        };
    }
    need
}

/// Offset of the PCIe capability (0 = none).
pub(crate) fn pcie_cap(n: &Node) -> u8 {
    n.cap(id::PCIE).map_or(0, |c| c.offset)
}

/// Enables `nodes[idx]` (see the module text). `may_storage`: the caller is the NVMe driver (R8a).
/// `clear_msi`: the first enable since boot or since `disable` (stale enables are cleared only then).
/// `phys_limit`: for sizing a function the scan left unsized. Untouched functions get no access at all.
#[allow(clippy::too_many_arguments)]
pub fn enable(
    c: &dyn ConfigAccess,
    nodes: &[Node],
    idx: usize,
    dma: bool,
    may_storage: bool,
    clear_msi: bool,
    phys_limit: u64,
    wait: &mut dyn FnMut(u64),
) -> Outcome {
    let mut out = Vec::new();
    let Some(n) = nodes.get(idx) else { return Outcome::fail(out, EnableError::NotScanned) };
    if let Some(k) = n.untouched {
        return Outcome::fail(out, EnableError::Untouched(k));
    }
    if n.is_storage() && !may_storage {
        return Outcome::fail(out, EnableError::Storage);
    }
    let path = ancestors(nodes, idx);
    if let Some(&b) = path.iter().find(|&&p| nodes[p].untouched.is_some()) {
        return Outcome::fail(out, EnableError::BridgeUntouched(nodes[b].loc));
    }
    // Bridges to D0 first, root first: one in D3hot forwards nothing below it, not even the function's ID.
    for &p in path.iter().rev() {
        let b = &nodes[p];
        let (true, Some(pm)) = (b.bridge.is_some(), b.cap(id::PM)) else { continue };
        if let Err(e) = wake(c, b.loc, pm.offset, pcie_cap(b), wait, &mut out) {
            let e = match e {
                EnableError::PowerStuck { state } => EnableError::BridgeAsleep { at: b.loc, state },
                e => e,
            };
            return Outcome::fail(out, e);
        }
    }
    let at = n.loc;
    let id = c.read32(at, 0);
    if !vendor_present(id as u16) || id as u16 != n.vendor || (id >> 16) as u16 != n.device {
        return Outcome::fail(out, EnableError::Gone);
    }
    if let Some(pm) = n.cap(id::PM)
        && let Err(e) = wake(c, at, pm.offset, pcie_cap(n), wait, &mut out)
    {
        return Outcome::fail(out, e);
    }
    let sized = (n.bars.is_none() && bars::slots(n.header_type) > 0)
        .then(|| bars::size_bars(c, at, n.header_type, n.class >> 8 == 0x0600, phys_limit, &mut Vec::new()));
    let bars = sized.as_ref().or(n.bars.as_ref());
    if clear_msi {
        clear_stale_msi(c, n, &mut out);
    }

    let need = decode_needed(bars);
    // Root first, like Linux's recursion in pci_enable_bridge: a bridge forwards only once the one above does.
    for &p in path.iter().rev() {
        let b = &nodes[p];
        let Some(info) = b.bridge else { continue };
        let mut want = if dma { CMD_MASTER } else { 0 };
        if need & CMD_MEM != 0 && (info.mem.is_some() || info.prefetch.is_some()) {
            want |= CMD_MEM;
        }
        if need & CMD_IO != 0 && info.io.is_some() {
            want |= CMD_IO;
        }
        let old = modify_command(c, b.loc, |cur| (cur != 0xFFFF && cur | want != cur).then_some(cur | want));
        if old == 0xFFFF {
            return Outcome::fail(out, EnableError::Gone);
        }
        if old | want != old {
            out.push(Change::Bridge { at: b.loc, old, new: old | want });
        }
    }
    let set = need | CMD_INTX_OFF | if dma { CMD_MASTER } else { 0 };
    let old = modify_command(c, at, |cur| (cur != 0xFFFF && cur | set != cur).then_some(cur | set));
    if old == 0xFFFF {
        return Outcome::fail(out, EnableError::Gone);
    }
    if old | set != old {
        out.push(Change::Command { old, new: old | set });
    }
    check_windows(nodes, &path, bars, &mut out);
    Outcome { changes: out, sized, saved: Some(save(c, at, pcie_cap(n))), error: None }
}

const MSI_ENABLE: u32 = 1 << 16;
const MSIX_ENABLE: u32 = 1 << 31;

/// Clears the enable bit `bit` in the capability dword at `off` if it is on; true if it was.
fn clear_bit(c: &dyn ConfigAccess, at: Loc, off: u8, bit: u32) -> bool {
    let mut was_on = false;
    c.modify32(at, off as u16, &mut |d| {
        was_on = d != u32::MAX && d & bit != 0;
        was_on.then_some(d & !bit)
    });
    was_on
}

/// Clears MSI enable (control bit 0) and MSI-X enable (control bit 15) where they are on.
fn clear_stale_msi(c: &dyn ConfigAccess, n: &Node, out: &mut Vec<Change>) {
    if let Some(cap) = n.cap(id::MSI)
        && clear_bit(c, n.loc, cap.offset, MSI_ENABLE)
    {
        out.push(Change::MsiCleared);
    }
    if let Some(cap) = n.cap(id::MSIX)
        && clear_bit(c, n.loc, cap.offset, MSIX_ENABLE)
    {
        out.push(Change::MsixCleared);
    }
}

/// Every assigned BAR must lie in a window of every bridge above (subtractive-decode bridges forward
/// everything and are not checked). Only reported: the firmware placed it, the kernel does not move BARs.
fn check_windows(nodes: &[Node], path: &[usize], bars: Option<&Bars>, out: &mut Vec<Change>) {
    for (i, b) in bars.into_iter().flat_map(|b| b.iter().enumerate()) {
        let Some(b) = b.filter(Bar::assigned) else { continue };
        let (start, end) = (b.start(), b.start().saturating_add(b.size().saturating_sub(1)));
        for &p in path {
            let br = &nodes[p];
            let Some(info) = br.bridge else { continue };
            if br.class == 0x06_04_01 {
                continue;
            }
            let inside = |w: Option<crate::node::Window>| w.is_some_and(|w| w.start <= start && end <= w.end);
            let ok = match b {
                Bar::Mem { .. } => inside(info.mem) || inside(info.prefetch),
                Bar::Io { .. } => inside(info.io),
            };
            if !ok {
                out.push(Change::BarOutsideWindow { index: i as u8, bridge: br.loc, start });
                break;
            }
        }
    }
}

/// What `disable` found and switched off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Disabled {
    pub msi_was_on: bool,
    pub msix_was_on: bool,
    pub bus_master_was_on: bool,
    /// A configuration lock stayed busy past the bound (a CPU stopped inside a configuration access): an
    /// access was skipped, so something may still be on. Set by `PciFunction::disable_device`.
    pub incomplete: bool,
    /// Nothing was written: an untouched function (R8d) or mass storage not bound to the NVMe driver (R8a).
    pub refused: bool,
}

/// For `Quiesce`: MSI and MSI-X off first, then bus master off. Memory decoding stays (the driver's BAR
/// mappings and the firmware's console may still be read), INTx stays off, bridges stay as they are (other
/// functions below them may still need them). No allocation, no logging, no waiting.
/// `msi`/`msix`: capability offsets (0 = none).
pub fn disable(c: &dyn ConfigAccess, at: Loc, msi: u8, msix: u8) -> Disabled {
    let mut d = Disabled::default();
    if msi != 0 {
        d.msi_was_on = clear_bit(c, at, msi, MSI_ENABLE);
    }
    if msix != 0 {
        d.msix_was_on = clear_bit(c, at, msix, MSIX_ENABLE);
    }
    modify_command(c, at, |cmd| {
        d.bus_master_was_on = cmd != 0xFFFF && cmd & CMD_MASTER != 0;
        d.bus_master_was_on.then_some(cmd & !CMD_MASTER)
    });
    d
}

/// The diagnosis rule of spec/usb.md §8 ("link up but nothing arrives"): command, status and, for PCIe,
/// device status of one function on the DMA path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaStatus {
    pub at: Loc,
    pub command: u16,
    pub status: u16,
    /// Bridges: secondary status (0x1E), where a master abort from below shows.
    pub secondary_status: Option<u16>,
    /// PCIe device status (capability 0x10 + 0x0A): bit 3 Unsupported Request detected, bit 5 transactions
    /// pending.
    pub pcie_dev_status: Option<u16>,
}

impl DmaStatus {
    pub fn bus_master(&self) -> bool {
        self.command & CMD_MASTER != 0
    }
    /// Received Master Abort (status bit 13) here or on a bridge's secondary side, or UR detected.
    pub fn trouble(&self) -> bool {
        self.status & (1 << 13) != 0
            || self.secondary_status.is_some_and(|s| s & (1 << 13) != 0)
            || self.pcie_dev_status.is_some_and(|s| s & (1 << 3) != 0)
    }
}

/// `DmaStatus` of the function and every bridge above it (function first).
pub fn dma_status(c: &dyn ConfigAccess, nodes: &[Node], idx: usize) -> Vec<DmaStatus> {
    let mut v = Vec::new();
    for i in core::iter::once(idx).chain(ancestors(nodes, idx)) {
        let n = &nodes[i];
        let d = c.read32(n.loc, 4);
        v.push(DmaStatus {
            at: n.loc,
            command: d as u16,
            status: (d >> 16) as u16,
            secondary_status: n.bridge.map(|_| read16(c, n.loc, 0x1E)),
            pcie_dev_status: n.cap(id::PCIE).map(|p| read16(c, n.loc, p.offset as u16 + 0x0A)),
        });
    }
    v
}
