//! The kernel's `Hw`: BAR0 through `hal::mmio::IoMem` (uncached, every access 4 bytes wide as both kinds want
//! them), the clock from `time`, config space through `pci`. Enable, disable, the D3hot wake and the restore of
//! lost BARs are PC-2's `enable_device`, `disable_device` and `restore_config`. The bring-up restores before it
//! enables, so the saved copy is the one the last `enable_device` (or probe's `save_config`) took while the
//! card was good, never one with the BARs a retrain zeroed. ASPM off stays here until PC-5's L1.2 helper.

use crate::hw::{Hw, IrqHook, PciFail};
use crate::irq::IrqSetup;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use hal::mmio::IoMem;
use ksync::SpinLock;
use mm::dma::{DmaBuffer, DmaCache, DmaConstraints};

/// `pause_us` sleeps from this length on and spins below it.
const SLEEP_MIN_US: u64 = 50;

const CAP_MSI: u8 = 0x05;
const CAP_PCIE: u8 = 0x10;
const CAP_MSIX: u8 = 0x11;

pub struct MmioHw {
    io: IoMem,
    pci: pci::PciFunction,
    /// Capability offsets found at probe (0 = absent), so Quiesce does not walk config space.
    msi: u8,
    msix: u8,
    pcie: u8,
    /// PC-3's interrupt set of the last bring-up (NW-4). Dropped outside the lock (dropping may sleep).
    irqs: SpinLock<Option<pci::IrqSet>>,
}

impl MmioHw {
    /// Maps BAR0 (its whole size; the I226's PTM_CTRL at 0x12540 must lie inside).
    pub fn map(pci: &pci::PciFunction) -> Result<MmioHw, String> {
        let Some(pci::Bar::Mem { phys, size, .. }) = pci.bar(0) else {
            return Err("BAR0 is not a memory BAR".into());
        };
        if size < 0x2_0000 {
            return Err(format!("BAR0 is {size:#x} bytes, want at least 128 KiB"));
        }
        let io = hal::mmio::map_device(phys, size as usize, hal::paging::CacheType::Uncached)
            .map_err(|e| format!("map BAR0 {phys:#x}+{size:#x}: {e:?}"))?;
        let caps = pci.capabilities();
        let find = |id: u8| caps.iter().find(|c| c.1 == id).map_or(0, |c| c.0);
        // The header as probe finds it is the first copy `restore_config` writes back (every `enable_device`
        // takes a newer one). Without it (the PCI core's enable lock stayed busy) a restore only logs
        // `restore.no_copy` and the card still comes up.
        if pci.save_config().is_none() {
            klog::event!(Warn, "net", "igc.save_config_failed", function = alloc::format!("{}", pci.loc());
                "no saved header until the first enable");
        }
        Ok(MmioHw { io, msi: find(CAP_MSI), msix: find(CAP_MSIX), pcie: find(CAP_PCIE), pci: pci.clone(), irqs: SpinLock::named("igc.irqs", None) })
    }
    pub fn pci(&self) -> &pci::PciFunction {
        &self.pci
    }
    /// The interrupt set's mode and per-index numbers (ktests, hw_info has them too).
    pub fn irq_stats(&self) -> Option<(pci::IrqMode, Vec<pci::IrqStats>, bool)> {
        let g = self.irqs.lock();
        let set = g.as_ref()?;
        Some((set.mode(), (0..set.len()).filter_map(|i| set.stats(i)).collect(), set.is_released()))
    }
    /// (MSI enabled, MSI-X enabled) as config space says now.
    pub fn msi_enabled(&self) -> (bool, bool) {
        let ctl = |off: u8| if off == 0 { 0 } else { self.pci.read32(off) >> 16 };
        (ctl(self.msi) & 1 != 0, ctl(self.msix) & 0x8000 != 0)
    }
}

impl Hw for MmioHw {
    type Mem = DmaBuffer;
    fn rd(&self, off: usize) -> u32 {
        self.io.read32(off).unwrap_or(!0)
    }
    fn wr(&self, off: usize, v: u32) {
        let _ = self.io.write32(off, v);
    }
    fn now_ns(&self) -> u64 {
        time::now_ns()
    }
    fn delay_us(&self, us: u64) {
        time::delay_us(us)
    }
    fn pause_us(&self, us: u64) {
        // Short steps (an MDIC poll) spin: a trip through the scheduler costs more than they wait.
        if us >= SLEEP_MIN_US { sched::sleep_ns(us * 1000) } else { time::delay_us(us) }
    }
    fn relax(&self) {
        sched::yield_now()
    }
    fn pci_command(&self) -> u16 {
        self.pci.command()
    }
    fn set_pci_command(&self, v: u16) {
        self.pci.set_command(v)
    }
    fn enable(&self, dma: bool) -> Result<(), PciFail> {
        // The PCI core logs every change and every failure (`pci enable ...`); nothing to add here.
        self.pci.enable_device(dma).map(|_| ()).map_err(|e| PciFail {
            refused: matches!(e, pci::EnableError::Untouched(_) | pci::EnableError::Storage | pci::EnableError::BridgeUntouched(_)),
            text: format!("enable_device({dma}): {e}"),
        })
    }
    fn pci_off(&self) {
        let _ = self.pci.disable_device();
    }
    fn dma_check(&self) {
        let _ = self.pci.dma_status();
    }
    fn aspm_off(&self) {
        if self.pcie != 0 {
            // Link Control (low half) ASPM bits 1:0 off; Link Status (high half) written 0 (its RW1C bits stay).
            let off = self.pcie + 0x10;
            let v = self.pci.read32(off);
            if v & 3 != 0 {
                self.pci.write32(off, v & 0xFFFC);
            }
        }
    }
    fn restore_config(&self) -> usize {
        // Errors (function gone, refused) are logged by the PCI core; the `enable` after this reports them.
        let n = self.pci.restore_config().unwrap_or(0);
        // `restore_config` adds bus master only while the PCI core's record says "enabled for DMA"; that record
        // is cleared by `disable_device` without waiting for its lock, so a busy lock could leave it set. The
        // rings are not reset yet: bus master goes off again. Checked whatever was written (one config read):
        // the trait promises "never leaves bus master on", not "never turns it on".
        if self.pci.command() & crate::hw::cmd::BUS_MASTER != 0 {
            let _ = self.pci.disable_device();
        }
        n
    }
    fn setup_irqs(&self, max: u16, hook: IrqHook) -> IrqSetup {
        let old = self.irqs.lock().take();
        drop(old);
        // No polled set from the PCI core: polling is the stack's own fallback (the poll thread's 50/500 µs).
        let req = pci::IrqRequest::new("igc", 1, max).modes(true, true, false).hook(hook);
        match self.pci.alloc_irqs(&req) {
            Ok(set) => {
                let m = match set.mode() {
                    pci::IrqMode::MsiX => IrqSetup::MsiX(set.len() as u16),
                    pci::IrqMode::Msi => IrqSetup::Msi,
                    pci::IrqMode::Polled => IrqSetup::Polled,
                };
                *self.irqs.lock() = Some(set);
                m
            }
            Err(e) => {
                klog::event!(Warn, "net", "igc.irq_fallback", function = alloc::format!("{}", self.pci.loc()), error = alloc::format!("{e}");
                    "no MSI-X or MSI vectors: polled");
                IrqSetup::Polled
            }
        }
    }
    fn irqs_degraded(&self) -> bool {
        let g = self.irqs.lock();
        match g.as_ref() {
            None => true,
            Some(s) => s.is_released() || (0..s.len()).any(|i| s.stats(i).is_some_and(|x| x.stormed)),
        }
    }
    fn alloc_dma(&self, len: usize) -> Result<DmaBuffer, String> {
        // 64-bit descriptors and buffers: no DMA32 needed. WriteBack (snooped) so tail writes need no
        // `write_barrier`; spelled out, not defaulted.
        let c = DmaConstraints { limit: u64::MAX, cache: DmaCache::WriteBack, ..DmaConstraints::default() };
        DmaBuffer::alloc(len, c).map_err(|e| format!("DMA {len} bytes: {e:?}"))
    }
}
