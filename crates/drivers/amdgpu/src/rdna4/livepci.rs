//! Wave 42: the PCI core as `hostprep`'s chain reaches it on the card (`hostprep::PciSide`): reads of the card's
//! own function and of every bridge above it, and the three things that change something, all on the card's own
//! function: AtomicOp requester enable (for `nbif`), the PCI core's enable and the MSI vector (for `ih`). The
//! decisions are `hostprep`'s; nothing here decides.

use super::hostprep::{self, BridgeNow, CardNow, For, PortNow, Prep};
use super::liveih;
use crate::devs::Device;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

const CAP_PM: u8 = 0x01;
const CAP_PCIE: u8 = 0x10;

/// The boot's DMAR survey. Stricter than the PCI core's gate (`acpi::dma_gate`, which lets a driver start when a
/// unit's state could not be read): this card is handed system memory only on a plain yes.
pub fn dma_survey() -> Result<(), String> {
    use acpi::dmar::DmaVerdict;
    match acpi::dmar::verdict() {
        DmaVerdict::NoTable | DmaVerdict::Free => Ok(()),
        v => Err(String::from(acpi::dmar::verdict_text(v))),
    }
}

fn d_state(f: &pci::PciFunction) -> u8 {
    f.find_capability(CAP_PM).map_or(0, |c| (f.read16(c + 4) & 3) as u8)
}

fn port(f: &pci::PciFunction) -> Option<PortNow> {
    let c = f.find_capability(CAP_PCIE)?;
    let flags = f.read16(c + hostprep::PCIE_FLAGS);
    // A version 1 capability ends before Device Capabilities 2: Linux reads 0 there (pcie_capability_read_dword).
    let v2 = flags & 0xF >= 2;
    Some(PortNow {
        port_type: ((flags >> 4) & 0xF) as u8,
        devcap2: if v2 { f.read32(c + hostprep::PCIE_DEVCAP2) } else { 0 },
        devctl2: if v2 { f.read16(c + hostprep::PCIE_DEVCTL2) } else { 0 },
    })
}

struct LivePci<'a> {
    dev: &'a Device,
}

impl LivePci<'_> {
    fn f(&self) -> Result<&pci::PciFunction, String> {
        self.dev.pci.as_ref().ok_or_else(|| String::from("no PCI function"))
    }
}

impl hostprep::PciSide for LivePci<'_> {
    fn survey(&mut self) -> Result<(), String> {
        dma_survey()
    }

    fn card(&mut self) -> Option<CardNow> {
        let f = self.dev.pci.as_ref()?;
        let command = f.command();
        if command == 0xFFFF {
            return None;
        }
        let nodes = pci::tree();
        let bars = nodes.iter().find(|n| n.loc == f.loc()).and_then(|n| n.bars.as_ref());
        Some(CardNow {
            command,
            d_state: d_state(f),
            bars_recorded: [0, 2, 5].iter().all(|i| super::live::scan_bar(&nodes, f.loc(), *i).is_some()),
            decode_needed: pci::enable::decode_needed(bars),
            pcie: port(f),
        })
    }

    fn bridges(&mut self) -> Vec<BridgeNow> {
        let Some(f) = self.dev.pci.as_ref() else { return Vec::new() };
        let nodes = pci::tree();
        f.upstream_bridges()
            .iter()
            .map(|b| {
                // The windows as the PCI core's enable will see them: the scan's record.
                let info = nodes.iter().find(|n| n.loc == b.loc()).and_then(|n| n.bridge);
                BridgeNow {
                    loc: format!("{}", b.loc()),
                    command: b.command(),
                    d_state: d_state(b),
                    mem_window: info.is_some_and(|i| i.mem.is_some() || i.prefetch.is_some()),
                    io_window: info.is_some_and(|i| i.io.is_some()),
                    pcie: port(b),
                }
            })
            .collect()
    }

    fn pages(&mut self, bytes: u64) -> Option<u64> {
        liveih::pages(self.dev, bytes)
    }

    fn arm_reload(&mut self) -> Result<(), String> {
        liveih::arm_quiesce(self.dev)
    }

    /// `pcie_capability_set_word(dev, PCI_EXP_DEVCTL2, PCI_EXP_DEVCTL2_ATOMIC_REQ)`, drivers/pci/pci.c:3822-3823.
    fn set_atomic_req(&mut self) -> Result<String, String> {
        let f = self.f()?;
        let off = f.find_capability(CAP_PCIE).ok_or("the card has no PCI Express capability")? + hostprep::PCIE_DEVCTL2;
        let old = f.read32(off);
        if old == u32::MAX {
            return Err(String::from("the card's Device Control 2 does not read"));
        }
        let old = old as u16;
        // The dword's upper half is Device Status 2, which defines no bit: written 0.
        if !f.write32_ext(u16::from(off), u32::from(old | hostprep::DEVCTL2_ATOMIC_REQ)) {
            return Err(String::from("the PCI core refused the write of the card's Device Control 2"));
        }
        let now = f.read16(off);
        klog::event!(Info, "gx", "rdna4.atomics", card = self.dev.index, was = klog::Hex(u64::from(old)), now = klog::Hex(u64::from(now)); "AtomicOp requester enable in the card's own Device Control 2");
        if now & hostprep::DEVCTL2_ATOMIC_REQ == 0 {
            return Err(format!("the card did not take AtomicOp requester enable (Device Control 2 reads {now:#06x})"));
        }
        Ok(format!("AtomicOps to the root port granted; requester enable set in the card's own Device Control 2 ({old:#06x} -> {now:#06x})"))
    }

    /// Linux's `pci_enable_device` and `pci_set_master` (amdgpu_drv.c:2441, ih_v7_0.c:399). `hostprep` asked for
    /// it only when it can change nothing but the card's own command register.
    fn enable(&mut self) -> Result<String, String> {
        let f = self.f()?;
        let before = f.command();
        let e = f.enable_device(true).map_err(|e| format!("{e:?}"))?;
        let what = format!("the card's command register {before:#06x} -> {:#06x}{}", e.command, if e.msi_cleared { ", a stale MSI enable cleared" } else { "" });
        if !e.bridges_bus_master.is_empty() {
            let locs: Vec<String> = e.bridges_bus_master.iter().map(|l| format!("{l}")).collect();
            klog::event!(Error, "gx", "rdna4.bridge_written", card = self.dev.index, bridges = locs.join(" ").as_str(); "the PCI core wrote a bridge of this card although each read as forwarding");
            return Err(format!("{what}; the PCI core switched bus master on at {} although each read as on before", locs.join(", ")));
        }
        Ok(what)
    }

    fn msi(&mut self) -> Result<String, String> {
        liveih::msi(self.dev)
    }
}

/// Before the session's lock: the PCI side of one stage call (`hostprep::prepare`).
pub fn prepare(dev: &Device, stage: For, would_run: bool) -> Prep {
    hostprep::prepare(&mut LivePci { dev }, stage, dev.display, would_run)
}
