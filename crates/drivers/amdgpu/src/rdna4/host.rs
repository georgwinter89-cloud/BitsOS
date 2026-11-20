//! Wave 42: what a stage needs beside the card's registers and VRAM, the machine's side: the DMAR survey's answer,
//! the card's PCI command register and the bridges above it, its one MSI vector, and system pages the card is told
//! about. Until this wave the card never needed system memory (every PSP buffer lies in VRAM); the GART's fault
//! page and the IH rings are the first.
//!
//! One object is both the card (`psp::Hw`) and its host side, so the wrappers a stage runs behind (`guard::Mc`,
//! `guard::Guard`, the bring-up owner) cover both: a host-side change passes the owner's gate and the stage's list
//! like a register write. Every method refuses by default, so a card with no host side changes nothing.
//!
//! On the card itself what needs PCI configuration space (`atomics`, `bus_master`, `bridges_forward`,
//! `enable_dma`, `msi`) was asked and done by `hostprep::prepare` before the stage runs, and these methods hand
//! its answers on: there `prepare` is the gate (it makes every check the stage makes before its first change,
//! in the stage's order), and what it changed stands in the stage's answer whether the stage passes or stops.

use super::guard::Mc;
use crate::gpucore::bringup::{Bringup, Gate};
use crate::gpucore::psp::Hw;
use alloc::string::String;

pub trait Host {
    /// May the card reach system pages at all? The boot's DMAR survey (`acpi::dma_gate`): with an IOMMU the
    /// firmware left translating, a card handed bus master would fault or write somewhere else.
    fn dma_survey(&mut self) -> Result<(), String> {
        Err(String::from("this card has no host side"))
    }
    /// Bit 2 of the card's own command register, read now.
    fn bus_master(&mut self) -> Option<bool> {
        None
    }
    /// The path is ready for the PCI core's enable to change nothing but the card's own command register (reads
    /// only): every bridge between the card and the CPU is in D0 and already has each bit the enable would set
    /// in it, the card is in D0 with its BARs on record. BitsOS never writes a bridge of this card: one that
    /// does not forward ends the stage.
    fn bridges_forward(&mut self) -> Result<(), String> {
        Err(String::from("this card has no host side"))
    }
    /// Linux's `pci_enable_atomic_ops_to_root` (drivers/pci/pci.c:3747-3826) for 32- and 64-bit AtomicOps, which
    /// gives it `have_atomics_support` on this card (amdgpu_device.c:4018-4021): the root port completes them and
    /// every switch port between routes them and blocks none (reads only), then AtomicOp requester enable in the
    /// card's OWN Device Control 2. Never a bridge. What it found and changed, in words.
    fn atomics(&mut self) -> Result<String, String> {
        Err(String::from("this card has no host side"))
    }
    /// The PCI core's enable of the card's own function for sending (Linux's `pci_enable_device` and
    /// `pci_set_master`): bus master, I/O decode, INTx off in its command register. Never a bridge, a BAR or a
    /// power state. What changed, in words.
    fn enable_dma(&mut self) -> Result<String, String> {
        Err(String::from("this card has no host side"))
    }
    /// The card's one MSI vector, allocated and enabled; where it arrives, in words.
    fn msi(&mut self) -> Result<String, String> {
        Err(String::from("this card has no host side"))
    }
    /// `bytes` of zeroed system memory in one piece, kept for the rest of the boot (the card may write there until
    /// it is reset): its address as the card sees it.
    fn pages(&mut self, bytes: u64) -> Option<u64> {
        let _ = bytes;
        None
    }
    fn sys_read(&mut self, at: u64, buf: &mut [u8]) -> bool {
        let _ = (at, buf);
        false
    }
    fn sys_write(&mut self, at: u64, data: &[u8]) -> bool {
        let _ = (at, data);
        false
    }
}

/// A card with its host side, as a stage of this wave takes it.
pub trait Card: Hw + Host {
    fn hw(&mut self) -> &mut dyn Hw;
}

impl<T: Hw + Host> Card for T {
    fn hw(&mut self) -> &mut dyn Hw {
        self
    }
}

impl<H: Host> Host for Mc<H> {
    fn dma_survey(&mut self) -> Result<(), String> {
        self.hw.dma_survey()
    }
    fn bus_master(&mut self) -> Option<bool> {
        self.hw.bus_master()
    }
    fn bridges_forward(&mut self) -> Result<(), String> {
        self.hw.bridges_forward()
    }
    fn atomics(&mut self) -> Result<String, String> {
        self.hw.atomics()
    }
    fn enable_dma(&mut self) -> Result<String, String> {
        self.hw.enable_dma()
    }
    fn msi(&mut self) -> Result<String, String> {
        self.hw.msi()
    }
    fn pages(&mut self, bytes: u64) -> Option<u64> {
        self.hw.pages(bytes)
    }
    fn sys_read(&mut self, at: u64, buf: &mut [u8]) -> bool {
        self.hw.sys_read(at, buf)
    }
    fn sys_write(&mut self, at: u64, data: &[u8]) -> bool {
        self.hw.sys_write(at, data)
    }
}

/// Behind a shut gate nothing of the host side changes either.
impl<H: Hw + Host> Host for Bringup<H> {
    fn dma_survey(&mut self) -> Result<(), String> {
        self.hw().dma_survey()
    }
    fn bus_master(&mut self) -> Option<bool> {
        self.hw().bus_master()
    }
    fn bridges_forward(&mut self) -> Result<(), String> {
        self.hw().bridges_forward()
    }
    fn atomics(&mut self) -> Result<String, String> {
        match self.gate() {
            Gate::Allow => self.hw().atomics(),
            Gate::Refuse(why) => Err(String::from(why)),
        }
    }
    fn enable_dma(&mut self) -> Result<String, String> {
        match self.gate() {
            Gate::Allow => self.hw().enable_dma(),
            Gate::Refuse(why) => Err(String::from(why)),
        }
    }
    fn msi(&mut self) -> Result<String, String> {
        match self.gate() {
            Gate::Allow => self.hw().msi(),
            Gate::Refuse(why) => Err(String::from(why)),
        }
    }
    fn pages(&mut self, bytes: u64) -> Option<u64> {
        match self.gate() {
            Gate::Allow => self.hw().pages(bytes),
            Gate::Refuse(_) => None,
        }
    }
    fn sys_read(&mut self, at: u64, buf: &mut [u8]) -> bool {
        self.hw().sys_read(at, buf)
    }
    fn sys_write(&mut self, at: u64, data: &[u8]) -> bool {
        self.gate() == Gate::Allow && self.hw().sys_write(at, data)
    }
}
