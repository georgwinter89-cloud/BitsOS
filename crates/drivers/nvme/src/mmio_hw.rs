//! The kernel's `Hw`: BAR0 through `hal::mmio::IoMem` (uncached, 4-byte accesses), the clock from `time`, DMA
//! from `mm::dma` (write-back, snooped, 64-bit: no barrier before a doorbell write, as the queue entries are
//! ordinary stores that x86 keeps in order before the uncached doorbell store), PCI enable/disable and the
//! interrupt vectors from PC-2/PC-3. Bus master on every bridge above the controller comes from
//! `enable_device(true)` (CONTRACTS 6b: the Mini-PC firmware leaves it off, DMA would fail with UR).

use crate::hw::{DmaMem, Hw, IrqMode};
use alloc::format;
use alloc::string::String;
use hal::mmio::IoMem;
use ksync::SpinLock;
use mm::dma::{DmaBuffer, DmaConstraints};

/// `pause_us` sleeps from this length on and spins below it.
const SLEEP_MIN_US: u64 = 50;
/// MSI-X storm bound: far above what one outstanding command per queue can cause, low enough that a
/// misbehaving device cannot hold a CPU (PC-3: the default 100 per ms would mask a busy NVMe queue).
const STORM: pci::StormLimit = pci::StormLimit { per_window: 5000, window_ns: 1_000_000 };

pub struct MmioHw {
    io: IoMem,
    pci: pci::PciFunction,
    irqs: SpinLock<Option<pci::IrqSet>>,
}

impl MmioHw {
    pub fn map(pci: &pci::PciFunction) -> Result<MmioHw, String> {
        let Some(pci::Bar::Mem { phys, size, .. }) = pci.bar(0) else {
            return Err("BAR0 is not a memory BAR".into());
        };
        // Registers 0..0x1000 plus the admin and first I/O queue's doorbells at a stride up to 1 KiB (DSTRD <= 8);
        // a larger stride is checked against the window in the bring-up, once CAP is read.
        if size < 0x2000 {
            return Err(format!("BAR0 is {size:#x} bytes, want at least 8 KiB"));
        }
        let io = hal::mmio::map_device(phys, size as usize, hal::paging::CacheType::Uncached).map_err(|e| format!("map BAR0 {phys:#x}+{size:#x}: {e:?}"))?;
        if pci.save_config().is_none() {
            klog::event!(Warn, "nvme", "save_config_failed", function = format!("{}", pci.loc()); "no saved header until the first enable");
        }
        Ok(MmioHw { io, pci: pci.clone(), irqs: SpinLock::named("nvme.irqs", None) })
    }
    pub fn pci(&self) -> &pci::PciFunction {
        &self.pci
    }
    /// (mode, per-index interrupt counts, released) of the current set.
    pub fn irq_stats(&self) -> Option<(pci::IrqMode, alloc::vec::Vec<u64>, bool)> {
        let g = self.irqs.lock();
        let s = g.as_ref()?;
        Some((s.mode(), (0..s.len()).map(|i| s.count(i)).collect(), s.is_released()))
    }
}

impl DmaMem for DmaBuffer {
    fn dev_addr(&self) -> u64 {
        self.device_addr()
    }
    fn size(&self) -> usize {
        self.len()
    }
    fn rd32(&self, off: usize) -> Option<u32> {
        self.read_u32(off)
    }
    fn wr64(&self, off: usize, v: u64) -> bool {
        self.write_u64(off, v)
    }
    fn copy_in(&self, off: usize, src: &[u8]) -> bool {
        DmaBuffer::copy_in(self, off, src)
    }
    fn copy_out(&self, off: usize, dst: &mut [u8]) -> bool {
        DmaBuffer::copy_out(self, off, dst)
    }
    fn zero(&self) -> bool {
        DmaBuffer::zero(self)
    }
}

impl Hw for MmioHw {
    type Mem = DmaBuffer;
    fn rd32(&self, off: usize) -> u32 {
        self.io.read32(off).unwrap_or(!0)
    }
    fn wr32(&self, off: usize, v: u32) {
        // Out of the window only for a stride the bring-up refused (it checks the doorbells against `window`).
        let _ = self.io.write32(off, v);
    }
    fn window(&self) -> usize {
        self.io.size()
    }
    fn now_ns(&self) -> u64 {
        time::now_ns()
    }
    fn pause_us(&self, us: u64, may_sleep: bool) {
        if may_sleep && us >= SLEEP_MIN_US { sched::sleep_ns(us * 1000) } else { time::delay_us(us) }
    }
    fn relax(&self) {
        sched::yield_now()
    }
    fn alloc_dma(&self, len: usize) -> Result<DmaBuffer, String> {
        DmaBuffer::alloc(len, DmaConstraints::dma64()).map_err(|e| format!("DMA {len} bytes: {e:?}"))
    }
    fn enable(&self, dma: bool) -> Result<(), String> {
        // Restore first (memory-only step): a reset may have left the header without its BAR.
        if !dma {
            let _ = self.pci.restore_config();
        }
        self.pci.enable_device(dma).map(|_| ()).map_err(|e| format!("enable_device({dma}): {e}"))
    }
    fn pci_off(&self) {
        let _ = self.pci.disable_device();
    }
    fn setup_irqs(&self, max: u16) -> IrqMode {
        self.free_irqs();
        // No polled set from the PCI core: the waiter polls by itself without vectors.
        let req = pci::IrqRequest::new("nvme", 1, max).modes(true, true, false).storm(Some(STORM));
        match self.pci.alloc_irqs(&req) {
            Ok(set) => {
                let m = match set.mode() {
                    pci::IrqMode::MsiX => IrqMode::MsiX(set.len() as u16),
                    pci::IrqMode::Msi => IrqMode::Msi,
                    pci::IrqMode::Polled => IrqMode::Polled,
                };
                *self.irqs.lock() = Some(set);
                m
            }
            Err(e) => {
                klog::event!(Warn, "nvme", "irq.fallback", function = format!("{}", self.pci.loc()), error = format!("{e}"); "no MSI-X or MSI: polled");
                IrqMode::Polled
            }
        }
    }
    fn free_irqs(&self) {
        let old = self.irqs.lock().take();
        drop(old);
    }
    fn wait_irq(&self, i: u16, timeout_ns: u64) -> bool {
        let h = {
            let g = self.irqs.lock();
            match g.as_ref() {
                Some(s) if !s.is_released() => s.handle(i as usize),
                _ => None,
            }
        };
        match h {
            Some(h) => {
                let _ = h.wait(timeout_ns);
                true
            }
            None => false,
        }
    }
    fn irq_count(&self, i: u16) -> u64 {
        self.irqs.lock().as_ref().map_or(0, |s| s.count(i as usize))
    }
}
