//! The kernel's `Bus` (rtw89-core's door to the chip): BAR2 through `hal::mmio::IoMem` (the whole
//! 1 MB window, uncached: R_AX_INDIR_ACCESS_ENTRY at 0x40000 lies inside), config space through
//! the PCI function, DMA memory from [`DmaPool`], time from `time`. The driver thread wraps it in
//! rtw89-core's `CmacRead` (rtw89_pci_ops_read32_cmac, WZ:linux pci.c:1998).
//!
//! Waits (B4): a poll reads every 2 us for its first [`SPIN_US`], then once per millisecond with
//! the thread asleep in between, up to the poll's own timeout (at most 10 s, `MAX_WAIT_MS`); a
//! delay of [`SPIN_US`] or more sleeps. In `quiet` mode (Quiesce: interrupts off, no sleeping)
//! every wait spins and a poll ends after at most [`QUIET_POLL_MS`].
//!
//! After Quiesce (`gone`, the card's `quiesced` flag) the thread's writes, polls, delays and DMA
//! allocations answer `BusError::Gone`, so a join or bring-up that held the chip lock ends at its
//! next access; a write to the command register can never switch memory decoding or bus master
//! back on (only Quiesce's own quiet accesses still pass).

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use hal::mmio::IoMem;
use rtw89_core::bus::{Bus, BusError, BusResult, MAX_WAIT_MS, MemWidth, Width};

use crate::dma::DmaPool;

/// Polls and delays spin this long before the thread sleeps
pub const SPIN_US: u64 = 100;
/// A poll in quiet mode (Quiesce) gives up after this
pub const QUIET_POLL_MS: u64 = 20;
const NS_PER_US: u64 = 1_000;
const NS_PER_MS: u64 = 1_000_000;

/// include/uapi/linux/pci_regs.h
pub const PCI_COMMAND: u16 = 0x04;
pub const CMD_MEMORY: u32 = 0x2;
pub const CMD_MASTER: u32 = 0x4;
/// The PCI Express capability's registers rtw89-core may write (DEVCTL2 at +0x28 inside)
pub const PCIE_CTL_LEN: u16 = 0x3c;

/// The config dwords rtw89-core may write: the command register (safety step, stop; header bytes
/// through cfg_write8 land there too) and the PCI Express capability's control registers. Never a
/// BAR (0x10-0x24) or the expansion ROM (0x30): a moved BAR would leave `IoMem` mapping the old
/// window; the MSI/PM capabilities and above stay the PCI core's.
pub fn cfg_write_allowed(off: u16, pcie: Option<u16>) -> bool {
    off & 3 == 0 && (off == PCI_COMMAND || pcie.is_some_and(|p| off >= p && off < p + PCIE_CTL_LEN))
}

/// After Quiesce (`gone`) a command write may not switch memory decoding or bus master on
/// (`cur`: the register now; writing a bit that is already on, or clearing bits, stays allowed)
pub fn command_write_ok(gone: bool, cur: u32, val: u32) -> bool {
    !gone || (val & !cur) & (CMD_MEMORY | CMD_MASTER) == 0
}

pub struct KBus {
    io: IoMem,
    pci: pci::PciFunction,
    pub dma: DmaPool,
    /// Quiesce: spin, never sleep, short polls
    pub quiet: bool,
    /// MMIO/config accesses refused (an address outside the window): counted for the status
    pub refused: u64,
    /// set by Quiesce (shared with the card): see the module text
    pub gone: Arc<AtomicBool>,
    /// short delays spun since the thread last slept (us): past SPIN_RUN_US it sleeps once, so a
    /// long run of 1 us steps (a C2H register wait) cannot hold a CPU at KernelRt
    spun_us: u64,
}

/// The longest run of spun short delays before the thread sleeps 1 ms (a delay only gets longer)
pub const SPIN_RUN_US: u64 = 2_000;

impl KBus {
    pub fn new(io: IoMem, pci: pci::PciFunction) -> KBus {
        KBus { io, pci, dma: DmaPool::default(), quiet: false, refused: 0, gone: Arc::new(AtomicBool::new(false)), spun_us: 0 }
    }

    /// The thread's accesses after Quiesce end here (Quiesce's own quiet ones pass)
    fn live(&self) -> BusResult<()> {
        if !self.quiet && self.gone.load(Ordering::Acquire) {
            return Err(BusError::Gone);
        }
        Ok(())
    }

    fn refused(&mut self, kind: &'static str, addr: u32, len: u32) -> BusError {
        self.refused += 1;
        BusError::Refused { kind, addr, len }
    }

    fn wait_us(&mut self, us: u64) {
        if self.quiet {
            time::delay_us(us);
        } else if us < SPIN_US {
            time::delay_us(us);
            self.spun_us += us.max(1);
            if self.spun_us >= SPIN_RUN_US {
                self.spun_us = 0;
                sched::sleep_ns(NS_PER_MS);
            }
        } else {
            self.spun_us = 0;
            sched::sleep_ns(us * NS_PER_US);
        }
    }
}

impl Bus for KBus {
    fn read(&mut self, w: Width, addr: u32) -> BusResult<u32> {
        let off = addr as usize;
        let r = match w {
            Width::W8 => self.io.read8(off).map(u32::from),
            Width::W16 => self.io.read16(off).map(u32::from),
            Width::W32 => self.io.read32(off),
        };
        r.map_err(|_| self.refused("mmio read", addr, u32::from(w.bytes())))
    }

    fn write(&mut self, w: Width, addr: u32, val: u32) -> BusResult<()> {
        self.live()?;
        let off = addr as usize;
        let r = match w {
            Width::W8 => self.io.write8(off, val as u8),
            Width::W16 => self.io.write16(off, val as u16),
            Width::W32 => self.io.write32(off, val),
        };
        r.map_err(|_| self.refused("mmio write", addr, u32::from(w.bytes())))
    }

    fn rmw(&mut self, w: Width, addr: u32, clear: u32, set: u32) -> BusResult<()> {
        let v = self.read(w, addr)?;
        self.write(w, addr, ((v & !clear) | set) & w.mask())
    }

    // read_poll_timeout as rtw89-core's bus::poll_loop, with the thread asleep between reads
    // after the first SPIN_US; the last read after the deadline decides (Linux's order)
    fn poll(&mut self, w: Width, addr: u32, mask: u32, want: u32, timeout_ms: u16) -> BusResult<()> {
        self.live()?;
        let mut limit_ms = u64::from(timeout_ms.min(MAX_WAIT_MS));
        if self.quiet {
            limit_ms = limit_ms.min(QUIET_POLL_MS);
        }
        let start = time::now_ns();
        let end = start.saturating_add(limit_ms * NS_PER_MS);
        let spin_end = start.saturating_add(SPIN_US * NS_PER_US);
        loop {
            if self.read(w, addr)? & mask == want {
                return Ok(());
            }
            let now = time::now_ns();
            if now >= end {
                let last = self.read(w, addr)?;
                if last & mask == want {
                    return Ok(());
                }
                return Err(BusError::PollTimeout { addr, width: w, mask, want, last });
            }
            if self.quiet || now < spin_end {
                time::delay_us(2);
            } else {
                sched::sleep_ns(NS_PER_MS.min(end - now));
                self.live()?;
            }
        }
    }

    fn delay_us(&mut self, us: u32) -> BusResult<()> {
        self.live()?;
        self.wait_us(u64::from(us).min(u64::from(MAX_WAIT_MS) * 1000));
        Ok(())
    }

    fn cfg_read(&mut self, off: u16) -> BusResult<u32> {
        if off & 3 != 0 {
            return Err(self.refused("cfg read", u32::from(off), 4));
        }
        if off < 0x100 {
            return Ok(self.pci.read32(off as u8));
        }
        match self.pci.read32_ext(off) {
            Some(v) => Ok(v),
            None => Err(self.refused("cfg read", u32::from(off), 4)),
        }
    }

    // Only `cfg_write_allowed` offsets; the command register under `command_write_ok`, and a
    // bus master write that met a Quiesce running at the same time is switched off again at once.
    fn cfg_write(&mut self, off: u16, val: u32) -> BusResult<()> {
        let pcie = self.pci.find_capability(0x10).map(u16::from);
        if !cfg_write_allowed(off, pcie) {
            return Err(self.refused("cfg write", u32::from(off), 4));
        }
        let gone = self.gone.load(Ordering::Acquire);
        if off == PCI_COMMAND {
            let cur = self.pci.read32(PCI_COMMAND as u8) & 0xffff;
            if !command_write_ok(gone, cur, val & 0xffff) {
                return Err(self.refused("cfg write after quiesce", u32::from(off), 4));
            }
            self.pci.write32(off as u8, val);
            if val & CMD_MASTER != 0 && self.gone.load(Ordering::SeqCst) {
                self.pci.disable_bus_master();
                return Err(BusError::Gone);
            }
            return Ok(());
        }
        self.live()?;
        self.pci.write32(off as u8, val);
        Ok(())
    }

    fn dma_alloc(&mut self, size: usize, align: usize) -> BusResult<u32> {
        self.live()?;
        self.dma.alloc(size, align)
    }
    // In quiet mode (Quiesce) nothing goes back to mm: no allocator lock with interrupts off; the
    // chunks are zeroed by the caller and the Reload or reboot takes the memory anyway.
    fn dma_free(&mut self, addr: u32, size: usize) -> BusResult<()> {
        if self.quiet {
            return Ok(());
        }
        self.dma.free(addr, size)
    }
    fn mem_write(&mut self, w: MemWidth, addr: u32, val: u64) -> BusResult<()> {
        match w {
            MemWidth::M8 => self.dma.write(addr, &[val as u8]),
            MemWidth::M32 => self.dma.write(addr, &(val as u32).to_le_bytes()),
            MemWidth::M64 => self.dma.write(addr, &val.to_le_bytes()),
        }
    }
    fn mem_read32(&mut self, addr: u32) -> BusResult<u32> {
        let mut b = [0u8; 4];
        self.dma.read(addr, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }
    fn mem_write_bytes(&mut self, addr: u32, data: &[u8]) -> BusResult<()> {
        self.dma.write(addr, data)
    }
    fn mem_read_bytes(&mut self, addr: u32, out: &mut [u8]) -> BusResult<()> {
        self.dma.read(addr, out)
    }
    fn now_ms(&mut self) -> u64 {
        time::now_ns() / NS_PER_MS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_writes_only_command_and_pcie_control() {
        let pcie = Some(0x70);
        assert!(cfg_write_allowed(PCI_COMMAND, pcie));
        for off in [0x00, 0x08, 0x0c, 0x10, 0x18, 0x24, 0x30, 0x3c, 0x50, 0xac, 0x100] {
            assert!(!cfg_write_allowed(off, pcie), "{off:#x}");
        }
        assert!(cfg_write_allowed(0x70 + 0x28, pcie), "DEVCTL2");
        assert!(!cfg_write_allowed(0x71, pcie), "unaligned");
        assert!(!cfg_write_allowed(0x98, None), "no PCIe capability");
    }

    #[test]
    fn after_quiesce_no_command_write_switches_dma_on() {
        // before Quiesce: the safety step sets bus master
        assert!(command_write_ok(false, 0x402, 0x406));
        // after: bus master or memory on refused, bus master off (stop, Quiesce) allowed
        assert!(!command_write_ok(true, 0x402, 0x406));
        assert!(!command_write_ok(true, 0x400, 0x402));
        assert!(command_write_ok(true, 0x406, 0x402));
        assert!(command_write_ok(true, 0x402, 0x402));
    }
}
