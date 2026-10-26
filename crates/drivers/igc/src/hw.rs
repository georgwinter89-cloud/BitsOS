//! What the driver logic needs from the machine: the card's registers, a clock, its PCI command register and a
//! few config-space steps, and DMA memory. The kernel implements it over `hal::mmio::IoMem` and `pci`
//! (`MmioHw`); host tests implement it with a register-level fake of the card (`fake.rs`), so reset, PHY and
//! ring logic run the same code in both.

use crate::irq::IrqSetup;
use crate::mem::DmaMem;
use alloc::string::String;
use alloc::sync::Arc;

/// A hard-IRQ hook (PC-3's `IrqHook`): runs with interrupts off, no heap, no klog, no plain spin lock.
pub type IrqHook = Arc<dyn Fn(u16) + Send + Sync>;

pub trait Hw: Send + Sync {
    type Mem: DmaMem + Send + Sync;
    /// 32-bit register read; all ones if the window is unusable (as a card gone from the bus reads).
    fn rd(&self, off: usize) -> u32;
    fn wr(&self, off: usize, v: u32);
    fn now_ns(&self) -> u64;
    /// Spins (bring-up and Quiesce run with interrupts possibly off; every wait has a deadline).
    fn delay_us(&self, us: u64);
    /// A wait in which the caller may sleep, so other threads and the pollers run meanwhile (the restart
    /// worker's bring-up through `Sleepy`). Default: spin as `delay_us`. Never used on the Quiesce path.
    fn pause_us(&self, us: u64) {
        self.delay_us(us)
    }
    /// One round of a busy wait for another thread's work (lets it run on the cooperative G0 scheduler).
    fn relax(&self) {
        core::hint::spin_loop()
    }
    fn pci_command(&self) -> u16;
    fn set_pci_command(&self, v: u16);
    /// PC-2 `enable_device(dma)`: D0, memory decoding, INTx off, and with `dma` bus master on the card **and on
    /// every bridge up to the root port** (the Mini-PC's root port 00:02.1 has it off, spec net.md §2.1). May
    /// sleep; never called on the Quiesce path or with a spin lock held.
    fn enable(&self, dma: bool) -> Result<(), PciFail>;
    /// PC-2 `disable_device`: MSI and MSI-X off, then bus master off; memory decoding stays. Lock-free, no klog,
    /// no sleeping (Quiesce runs it on the panic path). A Reload or a firmware may leave MSI or a ring on.
    fn pci_off(&self);
    /// The "link up but nothing moves" check: command and status of the card and every bridge above it, logged
    /// by the PCI core (a bridge without bus master or with an Unsupported Request shows there). Reads only.
    fn dma_check(&self) {}
    /// I226: ASPM L0s and L1 off in PCIe Link Control (packet loss in L1.2; PC-5 narrows it to L1.2).
    fn aspm_off(&self);
    /// PC-2 `restore_config`: the header dwords and PCIe control words that differ from the copy saved at the
    /// last `enable` (or at probe) are written back, memory decode off while the BARs move (a PCIe link retrain
    /// zeroes BAR0/BAR1 on the I226, V1 `nicpci.asm`). Never leaves bus master on. Returns how many were
    /// written (0: nothing was lost). A card asleep in D3hot is woken by `enable`, not here.
    fn restore_config(&self) -> usize;
    fn alloc_dma(&self, len: usize) -> Result<Self::Mem, String>;
    /// PC-3 `alloc_irqs` (NW-4): MSI-X with up to `max` vectors, else MSI, else nothing (the card is polled).
    /// A set left from an earlier bring-up (released by `pci_off`) is freed first. `hook` runs in hard-IRQ
    /// context with the index that fired. Called after `enable(true)`; may sleep; never on the Quiesce path.
    /// Default (fakes without interrupts): polled.
    fn setup_irqs(&self, _max: u16, _hook: IrqHook) -> IrqSetup {
        IrqSetup::Polled
    }
    /// The interrupts set up last no longer arrive as they should (released by `pci_off`, a line the PCI core's
    /// storm guard holds): the stack must poll the card meanwhile. Thread context.
    fn irqs_degraded(&self) -> bool {
        false
    }

    /// Read-modify-write helpers.
    fn set_bits(&self, off: usize, bits: u32) {
        self.wr(off, self.rd(off) | bits);
    }
    fn clear_bits(&self, off: usize, bits: u32) {
        self.wr(off, self.rd(off) & !bits);
    }
    /// Makes posted writes reach the card before a wait (`V1:igc.asm:160`).
    fn flush(&self) {
        let _ = self.rd(crate::regs::STATUS);
    }
}

/// The same machine, with every wait (`delay_us`) turned into `pause_us`: bring-up on the restart worker
/// thread, so a card that needs seconds (PHY semaphore held, resets, a dead MDIC) does not stall the poller
/// thread that every other driver and the timers share (B4).
pub struct Sleepy<'a, H: Hw>(pub &'a H);

impl<H: Hw> Hw for Sleepy<'_, H> {
    type Mem = H::Mem;
    fn rd(&self, off: usize) -> u32 {
        self.0.rd(off)
    }
    fn wr(&self, off: usize, v: u32) {
        self.0.wr(off, v)
    }
    fn now_ns(&self) -> u64 {
        self.0.now_ns()
    }
    fn delay_us(&self, us: u64) {
        self.0.pause_us(us)
    }
    fn pause_us(&self, us: u64) {
        self.0.pause_us(us)
    }
    fn relax(&self) {
        self.0.relax()
    }
    fn pci_command(&self) -> u16 {
        self.0.pci_command()
    }
    fn set_pci_command(&self, v: u16) {
        self.0.set_pci_command(v)
    }
    fn enable(&self, dma: bool) -> Result<(), PciFail> {
        self.0.enable(dma)
    }
    fn pci_off(&self) {
        self.0.pci_off()
    }
    fn dma_check(&self) {
        self.0.dma_check()
    }
    fn aspm_off(&self) {
        self.0.aspm_off()
    }
    fn restore_config(&self) -> usize {
        self.0.restore_config()
    }
    fn alloc_dma(&self, len: usize) -> Result<H::Mem, String> {
        self.0.alloc_dma(len)
    }
    fn setup_irqs(&self, max: u16, hook: IrqHook) -> IrqSetup {
        self.0.setup_irqs(max, hook)
    }
    fn irqs_degraded(&self) -> bool {
        self.0.irqs_degraded()
    }
}

/// `Hw::enable` refused or failed: `refused` (R8d/R8a, the function is not ours) is final, the rest is retried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PciFail {
    pub refused: bool,
    pub text: String,
}

/// Polls `cond` every `step_us` until it holds or `limit_us` passed (by the clock, never by a count alone).
pub fn until<H: Hw + ?Sized>(hw: &H, limit_us: u64, step_us: u64, mut cond: impl FnMut() -> bool) -> bool {
    let end = hw.now_ns().saturating_add(limit_us.saturating_mul(1000));
    loop {
        if cond() {
            return true;
        }
        if hw.now_ns() >= end {
            return cond();
        }
        hw.delay_us(step_us);
    }
}

/// PCI command register bits.
pub mod cmd {
    pub const MEMORY: u16 = 1 << 1;
    pub const BUS_MASTER: u16 = 1 << 2;
    pub const INTX_DISABLE: u16 = 1 << 10;
}
