//! Item 22: what the driver may do to a card's PCI side, as rules that code and tests ask. Linux's own probe changes
//! none of this on the Gaming-PC either (ReBAR is at its maximum, the firmware sized every bridge window:
//! `amdgpu_device_resize_fb_bar`, amdgpu_device.c:1091, is a no-op there).

use crate::gpucore::bringup::Gate;
use alloc::string::String;

/// The gate of every register owner of a card in this wave.
pub const GATE: Gate = Gate::Refuse("GP-GPU1: the card is attached read-only");

/// How far the card's DMA reaches: `dma_bits = 44` for GC 12.0.0 and 12.0.1 (gmc_v12_0.c:843). Wave 42 hands the
/// card its first system pages (the fault's default page, the IH rings): every one is checked against it.
pub const DMA_BITS: u32 = rdna4_core::layout::DMA_BITS;

/// What a wave may ask of the PCI side of a card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ask {
    /// Reading configuration space, BAR5 or BAR0.
    Read,
    /// Switching the function's bus master on (the card could then write system memory: plan/spec/gpu-rdna4.md section 5).
    BusMaster,
    /// Any BAR, ReBAR, bridge-window, ASPM or link change (R7): never, the firmware's layout works.
    Layout,
    /// A function-level or secondary-bus reset (it resets the card's own PCIe switch): never.
    Reset,
    /// A power-state change (D-state, BACO): not before the card's SMU is understood (R5).
    Power,
}

/// Whether a wave of the plan may do it. `stage` is the wave's number (G1 = 1, the read-only attach).
pub fn allowed(ask: Ask, stage: u8) -> Result<(), &'static str> {
    match ask {
        Ask::Read => Ok(()),
        Ask::BusMaster if stage >= 3 => Ok(()),
        Ask::BusMaster => Err("no bus master before the first DMA stage (G3): nothing may let the card write system memory"),
        Ask::Layout => Err("BARs, bridge windows and the link stay as the firmware set them"),
        Ask::Reset => Err("a reset would reset the card's own PCIe switch"),
        Ask::Power => Err("no power command before the card's SMU is understood"),
    }
}

/// Item 22's rule for everything that lets a card reach system memory (a system address named to it, bus master,
/// its MSI vector): only the driven card, and only on a plain yes of the boot's DMAR survey (`survey`: what
/// `host::Host::dma_survey` said). Asked where it happens: the stages `gmc` and `ih`, and the chain that enables
/// the function on the card (`hostprep::prepare`).
pub fn may_send(driven: bool, survey: Result<(), String>) -> Result<(), String> {
    if !driven {
        return Err(String::from("only the driven card is handed system memory, bus master or a vector"));
    }
    survey.map_err(|why| alloc::format!("DMAR survey: {why}"))
}

/// A system address the card could be handed later: inside the 44 bits.
pub fn dma_reachable(addr: u64, bytes: u64) -> bool {
    rdna4_core::layout::dma_reachable(addr, bytes)
}

/// Calls a read-only attach must not contain, checked against the sources by a host test (`tests/rdna4.rs`): PCI
/// configuration writes, the enable path (it writes the command register), interrupts, and every `Hw` write.
pub const FORBIDDEN_CALLS: [&str; 11] = [
    // `PciFunction::bar` sizes a function the scan left unsized (it writes the BARs): the scan's record is read instead.
    "bar",
    "write32",
    "write16",
    "write8",
    "enable_device",
    "restore_config",
    "alloc_irqs",
    "reg_write",
    "mem_write",
    "doorbell_write",
    "write_blind",
];
