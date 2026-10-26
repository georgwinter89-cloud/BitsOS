//! What the controller logic needs from the machine: registers, time, DMA memory, the PCI function's enable
//! and its interrupt vectors. The kernel's `MmioHw` and the host tests' fake controller implement it.

use alloc::string::String;

/// DMA memory as the queues see it. In the kernel a write-back `mm::dma::DmaBuffer` (snooped: no barrier
/// before a doorbell write), in host tests `VecMem`, which the fake reads and writes as its DMA. Every access
/// is bounds-checked: `None`/`false` is an error for the caller, never a panic.
pub trait DmaMem: Send + Sync {
    fn dev_addr(&self) -> u64;
    fn size(&self) -> usize;
    fn rd32(&self, off: usize) -> Option<u32>;
    fn wr64(&self, off: usize, v: u64) -> bool;
    fn copy_in(&self, off: usize, src: &[u8]) -> bool;
    fn copy_out(&self, off: usize, dst: &mut [u8]) -> bool;
    fn zero(&self) -> bool;
}

/// How completions reach the waiting thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqMode {
    /// MSI-X with this many vectors.
    MsiX(u16),
    Msi,
    /// No vectors (CONFIG `nvme_irq=off`, none granted, or host tests): the waiter polls.
    Polled,
}

impl IrqMode {
    pub fn name(self) -> &'static str {
        match self {
            IrqMode::MsiX(_) => "msix",
            IrqMode::Msi => "msi",
            IrqMode::Polled => "polled",
        }
    }
    pub fn vectors(self) -> u16 {
        match self {
            IrqMode::MsiX(n) => n,
            IrqMode::Msi => 1,
            IrqMode::Polled => 0,
        }
    }
}

pub trait Hw: Send + Sync {
    type Mem: DmaMem;
    /// A register read; a gone device answers all ones.
    fn rd32(&self, off: usize) -> u32;
    fn wr32(&self, off: usize, v: u32);
    /// Bytes of the register window (BAR0): the doorbells at the controller's stride must fit in it.
    fn window(&self) -> usize;
    fn now_ns(&self) -> u64;
    /// Waits about `us`: sleeps when `may_sleep`, else spins (Quiesce: interrupts off, other CPUs parked).
    fn pause_us(&self, us: u64, may_sleep: bool);
    /// Lets other threads run while a command is short of its completion.
    fn relax(&self);
    fn alloc_dma(&self, len: usize) -> Result<Self::Mem, String>;
    /// Memory decode on, with `dma` also bus master (PC-2's `enable_device`; bus master on the bridges above too).
    fn enable(&self, dma: bool) -> Result<(), String>;
    /// MSI/MSI-X and bus master off, vectors released (PC-2's `disable_device`): lock-free, no sleep.
    fn pci_off(&self);
    /// Asks for up to `max` vectors (index 0 = admin CQ). Drops a previous set first.
    fn setup_irqs(&self, max: u16) -> IrqMode;
    /// Frees the vectors (before a restart; dropping may sleep).
    fn free_irqs(&self);
    /// Waits on vector index `i` up to `timeout_ns`; false when there is no such vector (poll instead).
    fn wait_irq(&self, i: u16, timeout_ns: u64) -> bool;
    /// Interrupts seen on index `i` so far (0 without vectors).
    fn irq_count(&self, i: u16) -> u64;
}

/// Host-test memory: bytes at a made-up device address, shared so the fake controller reaches it by address.
#[cfg(test)]
#[derive(Clone)]
pub struct VecMem {
    pub addr: u64,
    pub bytes: alloc::sync::Arc<ksync::SpinLock<alloc::vec::Vec<u8>>>,
}

#[cfg(test)]
impl DmaMem for VecMem {
    fn dev_addr(&self) -> u64 {
        self.addr
    }
    fn size(&self) -> usize {
        self.bytes.lock().len()
    }
    fn rd32(&self, off: usize) -> Option<u32> {
        let b = self.bytes.lock();
        if !off.is_multiple_of(4) {
            return None;
        }
        Some(u32::from_le_bytes(b.get(off..off.checked_add(4)?)?.try_into().ok()?))
    }
    fn wr64(&self, off: usize, v: u64) -> bool {
        off.is_multiple_of(8) && self.copy_in(off, &v.to_le_bytes())
    }
    fn copy_in(&self, off: usize, src: &[u8]) -> bool {
        let mut b = self.bytes.lock();
        match off.checked_add(src.len()).and_then(|e| b.get_mut(off..e)) {
            Some(s) => {
                s.copy_from_slice(src);
                true
            }
            None => false,
        }
    }
    fn copy_out(&self, off: usize, dst: &mut [u8]) -> bool {
        let b = self.bytes.lock();
        match off.checked_add(dst.len()).and_then(|e| b.get(off..e)) {
            Some(s) => {
                dst.copy_from_slice(s);
                true
            }
            None => false,
        }
    }
    fn zero(&self) -> bool {
        self.bytes.lock().fill(0);
        true
    }
}
