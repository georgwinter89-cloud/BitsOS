//! What `device::Card` needs beyond the registers (`hw::Hw`): a clock, the card's PCI function, DMA memory, one
//! interrupt, the firmware file. The kernel implements it in `mmio_hw`; host tests with the fake card.

use crate::hw::Hw;
use crate::mem::DmaMem;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

/// A hard-IRQ hook (the PCI core's `IrqHook`): runs with interrupts off, no heap, no klog, no plain spin lock.
pub type IrqHook = Arc<dyn Fn(u16) + Send + Sync>;

/// `Machine::enable` refused or failed: `refused` (the function is not ours to touch) is final, the rest is retried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PciFail {
    pub refused: bool,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqSetup {
    Polled,
    Msi,
    MsiX,
}

impl IrqSetup {
    pub fn name(self) -> &'static str {
        match self {
            IrqSetup::Polled => "polled",
            IrqSetup::Msi => "msi",
            IrqSetup::MsiX => "msix",
        }
    }
}

/// What reading the firmware file gave.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FwFile {
    /// The bytes and where they came from.
    Found { path: String, bytes: Vec<u8> },
    /// No path had it; `why` lists each try.
    Missing { why: String },
}

pub trait Machine: Hw + Send + Sync {
    type Mem: DmaMem + Send + Sync;
    fn now_ns(&self) -> u64;
    /// A wait in which the caller may sleep (the worker's bring-up through `Sleepy`). Never used on the Quiesce path.
    fn pause_us(&self, us: u32) {
        self.delay_us(us)
    }
    /// One round of a busy wait for another thread's work.
    fn relax(&self) {
        core::hint::spin_loop()
    }
    /// D0, memory decoding, INTx off, and with `dma` bus master on the card and on every bridge above it. May sleep.
    fn enable(&self, dma: bool) -> Result<(), PciFail>;
    /// MSI and MSI-X off, then bus master off. Lock-free, no klog, no sleeping: Quiesce runs it on the panic path.
    fn pci_off(&self);
    /// The command register's bus master bit as it reads now.
    fn bus_master(&self) -> bool;
    /// ASPM L0s and L1 off in the card's PCIe Link Control.
    fn aspm_off(&self);
    /// Header and PCIe control words a link retrain lost, written back; never leaves bus master on. Returns how many.
    fn restore_config(&self) -> usize;
    /// Command and status of the card and every bridge above it, logged by the PCI core. Reads only.
    fn dma_check(&self) {}
    fn alloc_dma(&self, len: usize) -> Result<Self::Mem, String>;
    /// One vector, MSI-X before MSI; a set from an earlier bring-up is freed first. After `enable(true)`; may sleep.
    fn setup_irq(&self, _hook: IrqHook) -> IrqSetup {
        IrqSetup::Polled
    }
    /// The vector set up last no longer arrives as it should (released by `pci_off`, held by the storm guard).
    fn irq_degraded(&self) -> bool {
        false
    }
    /// The firmware file `name` (`rtl_nic/...`), from the initramfs or the stick. May sleep.
    fn firmware(&self, name: &str) -> FwFile;
}

/// The same registers with every wait turned into `pause_us`: bring-up on the worker thread, where the PHY's 50 ms
/// steps and the firmware's own delays must not stall the poller that every driver shares.
pub struct Sleepy<'a, M: Machine>(pub &'a M);

impl<M: Machine> Hw for Sleepy<'_, M> {
    fn rd8(&self, off: u16) -> u8 {
        self.0.rd8(off)
    }
    fn rd16(&self, off: u16) -> u16 {
        self.0.rd16(off)
    }
    fn rd32(&self, off: u16) -> u32 {
        self.0.rd32(off)
    }
    fn wr8(&self, off: u16, val: u8) {
        self.0.wr8(off, val)
    }
    fn wr16(&self, off: u16, val: u16) {
        self.0.wr16(off, val)
    }
    fn wr32(&self, off: u16, val: u32) {
        self.0.wr32(off, val)
    }
    fn delay_us(&self, us: u32) {
        self.0.pause_us(us)
    }
    fn cfg_rd8(&self, off: u16) -> Option<u8> {
        self.0.cfg_rd8(off)
    }
    fn cfg_wr8(&self, off: u16, val: u8) -> bool {
        self.0.cfg_wr8(off, val)
    }
    fn set_max_read_request_4096(&self) -> bool {
        self.0.set_max_read_request_4096()
    }
    fn phy_waits(&self) -> Option<&crate::ocp::PhyWaits> {
        self.0.phy_waits()
    }
}
