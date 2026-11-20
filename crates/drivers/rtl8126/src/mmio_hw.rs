//! The kernel's `Machine`: BAR2 through `hal::mmio::IoMem` (uncached; 8, 16 and 32-bit accesses kept apart, because
//! this card's registers are), the clock from `time`, config space, enable and disable through `pci`, DMA memory
//! from `mm::dma`, the firmware file through the VFS.

use crate::device::DMA_LIMIT;
use crate::hw::Hw;
use crate::machine::{FwFile, IrqHook, IrqSetup, Machine, PciFail};
use crate::ocp::PhyWaits;
use crate::regs;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use hal::mmio::IoMem;
use ksync::SpinLock;
use mm::dma::{DmaBuffer, DmaCache, DmaConstraints};

/// `pause_us` sleeps from this length on and spins below it (the 25 µs steps of a PHY access).
const SLEEP_MIN_US: u32 = 50;
const CAP_PCIE: u8 = 0x10;
/// PCIe Device Control: Max_Read_Request_Size, bits 14:12; 5 means 4096 bytes (`pcie_set_readrq`, pci.c).
const DEVCTL: u8 = 0x08;
const DEVCTL_READRQ: u32 = 0x7000;
const READRQ_4096: u32 = 5 << 12;
const LNKCTL: u8 = 0x10;
const CMD_BUS_MASTER: u16 = 1 << 2;

/// Where the firmware is looked for, in order: the initramfs (there before any USB runs, so the card does not
/// depend on the stick being readable), then the stick's firmware folder like every other driver's.
pub const FW_DIRS: [&str; 3] = ["/usr/lib/firmware", "/lib/firmware", "/boot/BITSOS2/firmware"];
/// The real file is 13 232 bytes; this only keeps a damaged stick from filling memory.
const FW_MAX: usize = 1 << 20;

/// Why the register window could not be had: the English text for the log, the German reason for the report page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MapFail {
    NoBar,
    Small(u64),
    Refused(String),
}

impl MapFail {
    pub fn text(&self) -> String {
        match self {
            MapFail::NoBar => "BAR2 is not a memory BAR".into(),
            MapFail::Small(size) => format!("BAR2 is {size:#x} bytes, want at least {:#x}", regs::MIN_BAR_SIZE),
            MapFail::Refused(why) => why.clone(),
        }
    }
    pub fn reason(&self) -> &'static str {
        match self {
            MapFail::NoBar => "ihr Adressbereich wurde nicht gefunden",
            MapFail::Small(_) => "ihr Adressbereich ist zu klein",
            MapFail::Refused(_) => "ihr Adressbereich lässt sich nicht einblenden",
        }
    }
}

/// The dword that holds config byte `off`, as far as anything may be built on it: all ones is what a function that
/// does not answer reads as (`ConfigAccess::read32`), and writing that back with one byte replaced would put 0xff
/// into its three neighbours.
pub(crate) fn usable_dword(read: Option<u32>) -> Option<u32> {
    read.filter(|v| *v != u32::MAX)
}

pub(crate) fn byte_of(dword: u32, off: u16) -> u8 {
    (dword >> ((off & 3) * 8)) as u8
}

pub(crate) fn with_byte(dword: u32, off: u16, val: u8) -> u32 {
    let shift = (off & 3) * 8;
    (dword & !(0xff << shift)) | (val as u32) << shift
}

pub struct MmioHw {
    io: IoMem,
    pci: pci::PciFunction,
    /// PCIe capability offset (0 = none).
    pcie: u8,
    phy_waits: PhyWaits,
    /// The interrupt set of the last `setup_irq`. Dropped outside the lock (dropping may sleep).
    irqs: SpinLock<Option<pci::IrqSet>>,
}

impl MmioHw {
    /// Maps BAR2, the register window (Linux takes the first memory BAR, M:5646-5653).
    pub fn map(pci: &pci::PciFunction) -> Result<MmioHw, MapFail> {
        let Some(pci::Bar::Mem { phys, size, .. }) = pci.bar(regs::REG_BAR) else {
            return Err(MapFail::NoBar);
        };
        if size < regs::MIN_BAR_SIZE as u64 {
            return Err(MapFail::Small(size));
        }
        let io = hal::mmio::map_device(phys, size as usize, hal::paging::CacheType::Uncached)
            .map_err(|e| MapFail::Refused(format!("map BAR2 {phys:#x}+{size:#x}: {e:?}")))?;
        let pcie = pci.find_capability(CAP_PCIE).unwrap_or(0);
        // The header as probe finds it is the first copy `restore_config` writes back.
        if pci.save_config().is_none() {
            klog::event!(Warn, "net", "rtl8126.save_config_failed", function = format!("{}", pci.loc()));
        }
        Ok(MmioHw { io, pci: pci.clone(), pcie, phy_waits: PhyWaits::new(), irqs: SpinLock::named("rtl8126.irqs", None) })
    }

    pub fn pci(&self) -> &pci::PciFunction {
        &self.pci
    }

    /// The interrupt set's mode and per-vector numbers (ktests).
    pub fn irq_stats(&self) -> Option<(pci::IrqMode, Vec<pci::IrqStats>, bool)> {
        let g = self.irqs.lock();
        let set = g.as_ref()?;
        Some((set.mode(), (0..set.len()).filter_map(|i| set.stats(i)).collect(), set.is_released()))
    }
}

impl Hw for MmioHw {
    fn rd8(&self, off: u16) -> u8 {
        self.io.read8(off as usize).unwrap_or(!0)
    }
    fn rd16(&self, off: u16) -> u16 {
        self.io.read16(off as usize).unwrap_or(!0)
    }
    fn rd32(&self, off: u16) -> u32 {
        self.io.read32(off as usize).unwrap_or(!0)
    }
    fn wr8(&self, off: u16, val: u8) {
        let _ = self.io.write8(off as usize, val);
    }
    fn wr16(&self, off: u16, val: u16) {
        let _ = self.io.write16(off as usize, val);
    }
    fn wr32(&self, off: u16, val: u32) {
        let _ = self.io.write32(off as usize, val);
    }
    fn delay_us(&self, us: u32) {
        time::delay_us(us as u64)
    }
    fn cfg_rd8(&self, off: u16) -> Option<u8> {
        usable_dword(self.pci.read32_ext(off & !3)).map(|v| byte_of(v, off))
    }
    /// The PCI core moves dwords: the byte is replaced inside its dword. Linux writes the byte alone when the bus
    /// has extended config space and otherwise does this same dword read-modify-write through the chip's CSI
    /// window (M:2902-2953), so the neighbours of both bytes are known to take it. A dword that cannot be read is
    /// not written (`usable_dword`); the sequence counts the setting as passed over.
    fn cfg_wr8(&self, off: u16, val: u8) -> bool {
        match usable_dword(self.pci.read32_ext(off & !3)) {
            Some(old) => self.pci.write32_ext(off & !3, with_byte(old, off, val)),
            None => false,
        }
    }
    fn set_max_read_request_4096(&self) -> bool {
        if self.pcie == 0 {
            return false;
        }
        // Device Status shares the dword: its bits are write-1-to-clear, so they are written 0.
        let off = self.pcie + DEVCTL;
        let ctl = self.pci.read32(off) & 0xffff;
        if ctl & DEVCTL_READRQ != READRQ_4096 {
            self.pci.write32(off, (ctl & !DEVCTL_READRQ) | READRQ_4096);
        }
        true
    }
    fn phy_waits(&self) -> Option<&PhyWaits> {
        Some(&self.phy_waits)
    }
}

impl Machine for MmioHw {
    type Mem = DmaBuffer;
    fn now_ns(&self) -> u64 {
        time::now_ns()
    }
    fn pause_us(&self, us: u32) {
        if us >= SLEEP_MIN_US { sched::sleep_ns(us as u64 * 1000) } else { time::delay_us(us as u64) }
    }
    fn relax(&self) {
        sched::yield_now()
    }
    fn enable(&self, dma: bool) -> Result<(), PciFail> {
        // The PCI core logs every change and every failure; nothing to add here.
        self.pci.enable_device(dma).map(|_| ()).map_err(|e| PciFail {
            refused: matches!(
                e,
                pci::EnableError::Untouched(_) | pci::EnableError::Storage | pci::EnableError::BridgeUntouched(_) | pci::EnableError::DmaBlocked(_)
            ),
            text: format!("enable_device({dma}): {e}"),
        })
    }
    fn pci_off(&self) {
        let _ = self.pci.disable_device();
    }
    fn bus_master(&self) -> bool {
        self.pci.command() & CMD_BUS_MASTER != 0
    }
    fn aspm_off(&self) {
        if self.pcie != 0 {
            // Link Control (low half) ASPM bits 1:0 off; Link Status (high half) written 0 (its RW1C bits stay).
            let off = self.pcie + LNKCTL;
            let v = self.pci.read32(off);
            if v & 3 != 0 {
                self.pci.write32(off, v & 0xFFFC);
            }
        }
    }
    fn restore_config(&self) -> usize {
        let n = self.pci.restore_config().unwrap_or(0);
        // "Never leaves bus master on": the chip is not reset yet (the same guard as igc's).
        if self.bus_master() {
            let _ = self.pci.disable_device();
        }
        n
    }
    fn dma_check(&self) {
        let _ = self.pci.dma_status();
    }
    fn alloc_dma(&self, len: usize) -> Result<DmaBuffer, String> {
        // Write-back (snooped), below `DMA_LIMIT`, page-aligned (the rings need 256 bytes).
        let c = DmaConstraints { limit: DMA_LIMIT, cache: DmaCache::WriteBack, ..DmaConstraints::dma32() };
        DmaBuffer::alloc(len, c).map_err(|e| format!("DMA {len} bytes: {e:?}"))
    }
    fn setup_irq(&self, hook: IrqHook) -> IrqSetup {
        let old = self.irqs.lock().take();
        drop(old);
        // Linux uses exactly one vector on this card (M:5347). No polled set from the PCI core: polling is the
        // stack's own fallback.
        let req = pci::IrqRequest::new("rtl8126", 1, 1).modes(true, true, false).hook(hook);
        match self.pci.alloc_irqs(&req) {
            Ok(set) => {
                let m = match set.mode() {
                    pci::IrqMode::MsiX => IrqSetup::MsiX,
                    pci::IrqMode::Msi => IrqSetup::Msi,
                    pci::IrqMode::Polled => IrqSetup::Polled,
                };
                *self.irqs.lock() = Some(set);
                m
            }
            Err(e) => {
                klog::event!(Warn, "net", "rtl8126.irq_fallback", function = format!("{}", self.pci.loc()), error = format!("{e}");
                    "no MSI-X or MSI vector: polled");
                IrqSetup::Polled
            }
        }
    }
    fn irq_degraded(&self) -> bool {
        let g = self.irqs.lock();
        match g.as_ref() {
            None => true,
            Some(s) => s.is_released() || (0..s.len()).any(|i| s.stats(i).is_some_and(|x| x.stormed)),
        }
    }
    fn firmware(&self, name: &str) -> FwFile {
        let ns = vfs::root_ns();
        let mut why = String::new();
        for dir in FW_DIRS {
            let path = format!("{dir}/{name}");
            match ns.read_file(&path, FW_MAX) {
                Ok(bytes) => return FwFile::Found { path, bytes },
                Err(e) => why.push_str(&format!("{path}: errno {e}; ")),
            }
        }
        FwFile::Missing { why }
    }
}
