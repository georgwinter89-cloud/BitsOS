//! The start-up safety check before Linux's order (not Linux; JS
//! js/bringup-steps.mjs `busMasterOn`, spec/wlan.md §2.3 "Startup safety"):
//! a killed run, a warm boot or V1 before a reboot may have left bus master or
//! the MAC on. Bus master goes off first, a MAC that is on is powered off
//! before DMA can start, then bus master on as pci_set_master does.
//! Precondition: memory space is on (the PCI layer's pci_enable_device).

use crate::bus::{Bus, field};
use crate::error::Rtw89Result;
use crate::tables::regs::*;

use super::power;

/// include/uapi/linux/pci_regs.h
pub const PCI_COMMAND: u16 = 0x04;
pub const PCI_COMMAND_MASTER: u32 = 0x4;

/// reg.h:332-334 (the emitted register list has only MAC_AX_MAC_ON)
pub const MAC_AX_MAC_OFF: u32 = 0;
pub const MAC_AX_MAC_LPS: u32 = 2;

/// What the safety check found and did
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Safety {
    /// the command register to restore at the stop (bus master off)
    pub cmd0: u32,
    /// bus master was on when we came
    pub was_master: bool,
    /// IC_PWR_STATE's MAC state when we came (0 off, 1 on, 2 LPS)
    pub mac_state: u32,
    /// the MAC was not off (on or LPS) and was powered off here
    pub powered_off: bool,
    /// IC_PWR_STATE read all ones: no verdict here (some registers read all
    /// ones on a live chip); a later step confirms with the PCI ID and
    /// R_AX_SYS_CFG1 (spec/wlan.md §2.3 "Dead-chip detection")
    pub dead_look: bool,
    /// the command register after bus master on (log read)
    pub cmd_now: u32,
}

// Linux: pci_set_master in rtw89_pci_claim_device (pci.c:3270, :3282), with the
// BitsOS safety step before it. Unlike the JS driver (MAC_ON only), LPS is
// powered off too: Linux's power-on only refuses MAC_ON (mac.c:4109), so a chip
// another system left in LPS would otherwise be powered on without a clean off.
pub fn bus_master_on<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<Safety> {
    let cmd = bus.cfg_read(PCI_COMMAND)? & 0xffff;
    let cmd0 = cmd & !PCI_COMMAND_MASTER;
    let was_master = cmd & PCI_COMMAND_MASTER != 0;
    if was_master {
        bus.cfg_write(PCI_COMMAND, cmd0)?;
    }
    let MacOff {
        mac_state,
        powered_off,
        dead_look,
    } = mac_off_if_on(bus)?;
    bus.cfg_write(PCI_COMMAND, cmd0 | PCI_COMMAND_MASTER)?;
    let cmd_now = match bus.cfg_read(PCI_COMMAND) {
        Ok(v) => v & 0xffff,
        Err(e) => {
            // the caller gets no Safety to undo, so bus master goes off again here
            let _ = bus.cfg_write(PCI_COMMAND, cmd0);
            return Err(e.into());
        }
    };
    Ok(Safety {
        cmd0,
        was_master,
        mac_state,
        powered_off,
        dead_look,
        cmd_now,
    })
}

/// What [`mac_off_if_on`] found and did
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacOff {
    /// IC_PWR_STATE's MAC state (0 off, 1 on, 2 LPS)
    pub mac_state: u32,
    /// the MAC was on or in LPS and was powered off here
    pub powered_off: bool,
    /// IC_PWR_STATE read all ones (no verdict, as in [`Safety::dead_look`])
    pub dead_look: bool,
}

/// The middle of the safety step: a MAC left on (or in LPS) powered off. Precondition: memory
/// space on and bus master off on the function. The kernel driver (WL-4) runs it once before the
/// PCI core may set bus master at all, so a MAC a Reload left running (its DMA engines still
/// holding the old kernel's ring addresses) never meets bus master on; the bring-up's own safety
/// step then finds the MAC off.
pub fn mac_off_if_on<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<MacOff> {
    let raw = bus.r32(R_AX_IC_PWR_STATE)?;
    let dead_look = raw == u32::MAX;
    let mac_state = field(raw, B_AX_WLMAC_PWR_STE_MASK);
    let powered_off = !dead_look && mac_state != MAC_AX_MAC_OFF;
    if powered_off {
        // rfe unknown before the efuse is read: 0, as the probe's power-on
        power::mac_pwr_off(bus, 0)?;
    }
    Ok(MacOff {
        mac_state,
        powered_off,
        dead_look,
    })
}
