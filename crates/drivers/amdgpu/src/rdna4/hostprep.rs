//! Wave 42: the PCI side of the stages `nbif` and `ih` on the card, as one chain of decisions over what the PCI
//! core says and does (`PciSide`). It is free of the kernel, so a host test drives it with a fake PCI side and
//! holds its bridge rule against the PCI core's own enable (`pci::enable::enable`).
//!
//! Why a chain before the stage: PCI's locks lie outside every driver lock (`ksync::order`) and a stage runs under
//! the session's lock, so what needs configuration space cannot happen inside the stage. On the card this chain
//! is therefore the gate for these changes, not the stage's guard (which only hands the answers on): it makes
//! every check the stage makes before its first change, in the stage's order, each step only after the one
//! before it passed, and what only asks comes before what changes. Everything it changed is kept in words
//! (`Prep::changes`) and stands in the stage's answer whether the stage then passes or stops; a stage that stops
//! later leaves these changes as they are (Linux's own probe makes them before any block starts).
//!
//! Never a bridge. `bridge_wants` is what the PCI core's enable would write into a bridge above the card
//! (crates/pci/src/enable.rs, the loop "Root first": bus master, and each decoding the card's BARs need where
//! the bridge has a window for it); a bridge that lacks one of these bits, or is not in D0 (the enable would wake
//! it), ends the chain before the enable is asked for.

use super::policy;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

pub const CMD_IO: u16 = 1 << 0;
pub const CMD_MEM: u16 = 1 << 1;
pub const CMD_MASTER: u16 = 1 << 2;

/// include/uapi/linux/pci_regs.h:485-490, :665-684 (offsets from the PCI Express capability).
pub const PCIE_FLAGS: u8 = 0x02;
pub const PCIE_DEVCAP2: u8 = 0x24;
pub const PCIE_DEVCTL2: u8 = 0x28;
pub const TYPE_ENDPOINT: u8 = 0x0;
pub const TYPE_LEG_END: u8 = 0x1;
pub const TYPE_ROOT_PORT: u8 = 0x4;
pub const TYPE_UPSTREAM: u8 = 0x5;
pub const TYPE_DOWNSTREAM: u8 = 0x6;
pub const DEVCAP2_ATOMIC_ROUTE: u32 = 0x40;
pub const DEVCAP2_ATOMIC_COMP32: u32 = 0x80;
pub const DEVCAP2_ATOMIC_COMP64: u32 = 0x100;
pub const DEVCTL2_ATOMIC_REQ: u16 = 0x40;
pub const DEVCTL2_ATOMIC_EGRESS_BLOCK: u16 = 0x80;

/// A function's PCI Express capability as it reads now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortNow {
    /// `PCI_EXP_FLAGS_TYPE`.
    pub port_type: u8,
    pub devcap2: u32,
    pub devctl2: u16,
}

/// A bridge above the card as it reads now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeNow {
    pub loc: String,
    pub command: u16,
    /// The power state of its PM capability (0 = D0, also without the capability).
    pub d_state: u8,
    /// The scan found a memory or prefetchable window, an I/O window.
    pub mem_window: bool,
    pub io_window: bool,
    pub pcie: Option<PortNow>,
}

/// The card's own function as it reads now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CardNow {
    pub command: u16,
    /// As a bridge's.
    pub d_state: u8,
    /// The scan recorded its BARs (else the PCI core's enable would size them, which writes them).
    pub bars_recorded: bool,
    /// The decodings its assigned BARs need (`pci::enable::decode_needed`).
    pub decode_needed: u16,
    pub pcie: Option<PortNow>,
}

/// What the PCI core and the kernel say and do for one card.
pub trait PciSide {
    /// The boot's DMAR survey.
    fn survey(&mut self) -> Result<(), String>;
    /// Reads only.
    fn card(&mut self) -> Option<CardNow>;
    /// The bridges from the card up to the root (nearest first), reads only.
    fn bridges(&mut self) -> Vec<BridgeNow>;
    /// Zeroed system memory for the card, kept for the boot.
    fn pages(&mut self, bytes: u64) -> Option<u64>;
    /// From here a Reload switches the card's IH rings off if one is on (registers a hook, writes nothing).
    fn arm_reload(&mut self) -> Result<(), String>;
    /// Writes AtomicOp requester enable into the card's own Device Control 2; what changed, in words.
    fn set_atomic_req(&mut self) -> Result<String, String>;
    /// The PCI core's enable of the card's function with bus master; what changed, in words.
    fn enable(&mut self) -> Result<String, String>;
    /// The card's one MSI vector; where it arrives, in words.
    fn msi(&mut self) -> Result<String, String>;
}

/// What the PCI core's enable would OR into this bridge's command register for a card whose BARs need `need`.
pub fn bridge_wants(need: u16, b: &BridgeNow) -> u16 {
    let mut want = CMD_MASTER;
    if need & CMD_MEM != 0 && b.mem_window {
        want |= CMD_MEM;
    }
    if need & CMD_IO != 0 && b.io_window {
        want |= CMD_IO;
    }
    want
}

/// Every bridge above the card already is as the PCI core's enable wants it, so the enable writes none of them.
pub fn bridges_forward(need: u16, bridges: &[BridgeNow]) -> Result<(), String> {
    for b in bridges {
        if b.command == 0xFFFF {
            return Err(format!("the bridge {} does not answer", b.loc));
        }
        if b.d_state != 0 {
            return Err(format!("the bridge {} is in D{}, not in D0, and BitsOS writes no bridge of this card", b.loc, b.d_state));
        }
        let missing = bridge_wants(need, b) & !b.command;
        if missing != 0 {
            let names: Vec<&str> = [(CMD_MASTER, "bus master"), (CMD_MEM, "memory decode"), (CMD_IO, "I/O decode")].iter().filter(|(bit, _)| missing & bit != 0).map(|(_, n)| *n).collect();
            return Err(format!("the bridge {} has {} off (command register {:#06x}), and BitsOS writes no bridge of this card", b.loc, names.join(" and "), b.command));
        }
    }
    Ok(())
}

/// The path is ready for the PCI core's enable to change nothing but the card's own command register: the
/// bridges as above, the card in D0 (no power-state change) with its BARs on record (none is sized).
pub fn path_ready(card: &CardNow, bridges: &[BridgeNow]) -> Result<(), String> {
    bridges_forward(card.decode_needed, bridges)?;
    if card.d_state != 0 {
        return Err(format!("the card is in D{}, not in D0: no power-state change is made for it", card.d_state));
    }
    if !card.bars_recorded {
        return Err(String::from("the PCI scan did not record the card's BARs: the enable would size them, which writes them"));
    }
    Ok(())
}

/// The reads of Linux's `pci_enable_atomic_ops_to_root` (drivers/pci/pci.c:3747-3820) for 32- and 64-bit
/// AtomicOps: the card is an endpoint, the root port completes both sizes, every switch port between routes
/// AtomicOps and no upstream port blocks them at egress. Linux's branch for a root port a hypervisor hides
/// (:3779-3793) is not taken over: without a root port the answer is no.
pub fn atomics_path(card: &CardNow, bridges: &[BridgeNow]) -> Result<(), String> {
    let own = card.pcie.ok_or("the card has no PCI Express capability")?;
    if own.port_type != TYPE_ENDPOINT && own.port_type != TYPE_LEG_END {
        return Err(format!("the card is no endpoint (port type {})", own.port_type));
    }
    let root = bridges.iter().rposition(|b| b.pcie.is_some_and(|p| p.port_type == TYPE_ROOT_PORT)).ok_or("no root port above the card")?;
    let want = DEVCAP2_ATOMIC_COMP32 | DEVCAP2_ATOMIC_COMP64;
    let cap = bridges[root].pcie.map_or(0, |p| p.devcap2);
    if cap & want != want {
        return Err(format!("the root port {} does not complete 32- and 64-bit AtomicOps (Device Capabilities 2 {cap:#x})", bridges[root].loc));
    }
    for b in &bridges[..root] {
        let Some(p) = b.pcie else { continue };
        if p.port_type == TYPE_UPSTREAM && p.devctl2 & DEVCTL2_ATOMIC_EGRESS_BLOCK != 0 {
            return Err(format!("the upstream port {} blocks AtomicOps at egress", b.loc));
        }
        if (p.port_type == TYPE_UPSTREAM || p.port_type == TYPE_DOWNSTREAM) && p.devcap2 & DEVCAP2_ATOMIC_ROUTE == 0 {
            return Err(format!("the switch port {} does not route AtomicOps", b.loc));
        }
    }
    Ok(())
}

/// Which stage a call is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum For {
    Nbif,
    /// With the bytes of system memory the stage will ask for (both rings and the write-back page).
    Ih { bytes: u64 },
    /// A stage with no PCI side.
    Other,
}

/// What the PCI side said and did for one call of a stage. `None`: not asked, a step before it said no.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Prep {
    pub atomics: Option<Result<String, String>>,
    pub bus_master: Option<bool>,
    pub path: Option<Result<(), String>>,
    /// (bytes, address) of the memory taken for the stage.
    pub pages: Option<(u64, u64)>,
    pub enabled: Option<Result<String, String>>,
    pub msi: Option<Result<String, String>>,
    /// What changed on the machine's side, in order, in words.
    pub changes: Vec<String>,
}

/// The chain. `driven`: the card is the display card. `would_run`: the stage's own order check passes (asked
/// under the session's lock by a caller that lets no other stage call of this card run meanwhile).
pub fn prepare(side: &mut dyn PciSide, stage: For, driven: bool, would_run: bool) -> Prep {
    let mut p = Prep::default();
    if !driven || !would_run {
        return p;
    }
    match stage {
        For::Other => {}
        For::Nbif => {
            let Some(card) = side.card() else {
                p.atomics = Some(Err(String::from("the card's configuration space does not read")));
                return p;
            };
            if let Err(why) = atomics_path(&card, &side.bridges()) {
                p.atomics = Some(Err(why));
                return p;
            }
            // Set already (a `nbif` that runs again after a stop): nothing to change.
            let ctl2 = card.pcie.map_or(0, |c| c.devctl2);
            if ctl2 & DEVCTL2_ATOMIC_REQ != 0 {
                p.atomics = Some(Ok(format!("AtomicOps to the root port granted; requester enable was set already (the card's own Device Control 2 {ctl2:#06x})")));
                return p;
            }
            p.atomics = Some(side.set_atomic_req());
            if let Some(Ok(what)) = &p.atomics {
                p.changes.push(what.clone());
            }
        }
        For::Ih { bytes } => {
            if policy::may_send(driven, side.survey()).is_err() {
                return p;
            }
            let Some(card) = side.card() else {
                p.path = Some(Err(String::from("the card's configuration space does not read")));
                return p;
            };
            p.path = Some(path_ready(&card, &side.bridges()));
            p.bus_master = Some(card.command & CMD_MASTER != 0);
            if p.path != Some(Ok(())) {
                return p;
            }
            let Some(at) = side.pages(bytes) else { return p };
            p.pages = Some((bytes, at));
            // Before the first change: a ring the stage switches on is switched off again at a Reload.
            if let Err(e) = side.arm_reload() {
                p.enabled = Some(Err(format!("a Reload could not switch the card's rings off ({e})")));
                return p;
            }
            p.enabled = Some(side.enable());
            let Some(Ok(what)) = &p.enabled else { return p };
            p.changes.push(what.clone());
            p.msi = Some(side.msi());
            if let Some(Ok(what)) = &p.msi {
                p.changes.push(format!("MSI enabled: {what}"));
            }
        }
    }
    p
}
