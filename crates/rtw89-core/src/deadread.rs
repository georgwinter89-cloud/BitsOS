//! Reads that may come from a dead chip (spec/wlan.md §2.3 "Dead-chip detection")
//! and Linux's CMAC read rule.
//!
//! - [`looks_dead`]: the patterns a gone or clock-gated chip answers. Not a verdict
//!   by itself: some registers read all ones on a live chip (DLE_EMPTY0).
//! - [`chip_answers`]: the verdict, as V1's `wl_answers` (health.asm:108): the PCI
//!   ID in config space and R_AX_SYS_CFG1, which answer in every power state.
//! - [`CmacRead`]: rtw89_pci_ops_read32_cmac (pci.c:1998) as a [`Bus`] wrapper. A
//!   CMAC register (0xC000-0xFFFF) that reads 0xDEADBEEF has its clock off: write
//!   R_AX_CK_EN and read again, up to MAC_REG_POOL_COUNT times. Linux does it in the
//!   HCI's read op, below every module, so the kernel driver (WL-4) wraps its MMIO
//!   bus in this. The JS driver does not have the rule, so the trace tests run
//!   without it (on a good run it changes nothing: no CMAC read is 0xDEADBEEF).

use crate::bus::{Bus, BusResult, MemWidth, Width, poll_loop};
use crate::error::Rtw89Error;
use crate::tables::regs::*;

/// reg.h:2143 and 3771: the CMAC's register window (ACCESS_CMAC, mac.h:587)
pub const R_AX_CMAC_REG_START: u32 = 0xC000;
pub const R_AX_CMAC_REG_END: u32 = 0xFFFF;
/// mac.h:586: how often rtw89_pci_ops_read32_cmac turns the clocks on and reads again
pub const MAC_REG_POOL_COUNT: u32 = 10;
/// Config dword 0: device 0xB852, vendor Realtek 0x10EC (V1 chip.asm:5 `WL_PCI_ID`)
pub const RTL8852BE_PCI_ID: u32 = 0xB852_10EC;

/// All ones (the link is gone), RTW89_R32_DEAD or RTW89_R32_EA (V1 health.asm
/// `wh_dead_value`; Linux: rtw89_pci_ltr_is_err_reg_val, pci.h:1731, plus all ones)
pub const fn looks_dead(v: u32) -> bool {
    v == u32::MAX || v == RTW89_R32_DEAD || v == RTW89_R32_EA
}

// Not Linux: V1 health.asm:108 `wl_answers`. True while the chip answers: config dword 0 is
// the RTL8852BE's ID and R_AX_SYS_CFG1 reads no dead pattern (Linux reads it before the
// power-on in rtw89_read_chip_ver, so it answers in every power state). A bus error is
// returned as it is (a gone bus is the caller's verdict already).
pub fn chip_answers<B: Bus + ?Sized>(bus: &mut B) -> BusResult<bool> {
    if bus.cfg_read(0)? != RTL8852BE_PCI_ID {
        return Ok(false);
    }
    Ok(!looks_dead(bus.r32(R_AX_SYS_CFG1)?))
}

impl Rtw89Error {
    /// The register and value of an error whose register read looks dead ([`looks_dead`]):
    /// the cue for the caller's health check to confirm with [`chip_answers`]. `None` for
    /// errors without a register read and for live-looking values.
    pub fn dead_read(&self) -> Option<(u32, u32)> {
        use crate::bus::BusError;
        let (reg, value) = match self {
            Rtw89Error::Bus(BusError::PollTimeout { addr, last, .. }) => (*addr, *last),
            Rtw89Error::MacAlreadyOn { ic_pwr_state } => (R_AX_IC_PWR_STATE, *ic_pwr_state),
            Rtw89Error::MacNotEnabled { reg, value, .. }
            | Rtw89Error::LtrDead { reg, value }
            | Rtw89Error::RingIndex { reg, value, .. }
            | Rtw89Error::WcpuAlreadyOn { reg, value }
            | Rtw89Error::FwPathNotReady { reg, value, .. }
            | Rtw89Error::FwStatus { reg, value, .. }
            | Rtw89Error::Ch12Stuck { reg, value, .. }
            | Rtw89Error::FwLinkGone { reg, value, .. }
            | Rtw89Error::H2cRegBusy { reg, value }
            | Rtw89Error::C2hRegTimeout { reg, value }
            | Rtw89Error::C2hRegAnswer { reg, value, .. }
            | Rtw89Error::RadioBlocked { reg, value } => (*reg, *value),
            _ => return None,
        };
        looks_dead(value).then_some((reg, value))
    }
}

/// ACCESS_CMAC (mac.h:587)
pub const fn access_cmac(addr: u32) -> bool {
    addr >= R_AX_CMAC_REG_START && addr <= R_AX_CMAC_REG_END
}

/// A bus whose reads of CMAC registers follow rtw89_pci_ops_read32_cmac; everything else
/// goes to the inner bus as it is. Linux's rtw89_read8/16/32 and the RMWs and polls built on
/// them all read through it, so `rmw` and `poll` of a CMAC register do here too.
pub struct CmacRead<B> {
    pub inner: B,
    /// reads that stayed 0xDEADBEEF after the last retry (Linux: "addr %#x = %#x" warning)
    pub dead_reads: u64,
    /// the last such register and value, for the caller's log (B6)
    pub last_dead: Option<(u32, u32)>,
}

impl<B: Bus> CmacRead<B> {
    pub fn new(inner: B) -> CmacRead<B> {
        CmacRead {
            inner,
            dead_reads: 0,
            last_dead: None,
        }
    }

    // Linux: rtw89_pci_ops_read32_cmac (pci.c:1998)
    fn read32_cmac(&mut self, addr: u32) -> BusResult<u32> {
        let mut val = self.inner.read(Width::W32, addr)?;
        let mut count = 0;
        loop {
            if val != RTW89_R32_DEAD {
                return Ok(val);
            }
            if count >= MAC_REG_POOL_COUNT {
                self.dead_reads = self.dead_reads.saturating_add(1);
                self.last_dead = Some((addr, val));
                return Ok(RTW89_R32_DEAD);
            }
            self.inner
                .write(Width::W32, R_AX_CK_EN, B_AX_CMAC_ALLCKEN)?;
            val = self.inner.read(Width::W32, addr)?;
            count += 1;
        }
    }
}

impl<B: Bus> Bus for CmacRead<B> {
    // Linux: rtw89_pci_ops_read8/16/32 (pci.c:2017-2050): a CMAC byte or word is taken
    // from its aligned dword
    fn read(&mut self, w: Width, addr: u32) -> BusResult<u32> {
        if !access_cmac(addr) {
            return self.inner.read(w, addr);
        }
        let v = self.read32_cmac(addr & !3)?;
        Ok((v >> ((addr & 3) * 8)) & w.mask())
    }
    fn write(&mut self, w: Width, addr: u32, val: u32) -> BusResult<()> {
        self.inner.write(w, addr, val)
    }
    // Linux: rtw89_write32_mask and friends read with rtw89_read32 (core.h:8224)
    fn rmw(&mut self, w: Width, addr: u32, clear: u32, set: u32) -> BusResult<()> {
        if !access_cmac(addr) {
            return self.inner.rmw(w, addr, clear, set);
        }
        let old = self.read(w, addr)?;
        self.inner.write(w, addr, ((old & !clear) | set) & w.mask())
    }
    fn poll(&mut self, w: Width, addr: u32, mask: u32, want: u32, ms: u16) -> BusResult<()> {
        if !access_cmac(addr) {
            return self.inner.poll(w, addr, mask, want, ms);
        }
        // read_poll_timeout's step in the CMAC polls of mac.c (1 us .. 10 us): 10 us
        poll_loop(self, w, addr, mask, want, ms, 10)
    }
    fn delay_us(&mut self, us: u32) -> BusResult<()> {
        self.inner.delay_us(us)
    }
    fn cfg_read(&mut self, off: u16) -> BusResult<u32> {
        self.inner.cfg_read(off)
    }
    fn cfg_write(&mut self, off: u16, val: u32) -> BusResult<()> {
        self.inner.cfg_write(off, val)
    }
    fn dma_alloc(&mut self, size: usize, align: usize) -> BusResult<u32> {
        self.inner.dma_alloc(size, align)
    }
    fn dma_free(&mut self, addr: u32, size: usize) -> BusResult<()> {
        self.inner.dma_free(addr, size)
    }
    fn mem_write(&mut self, w: MemWidth, addr: u32, val: u64) -> BusResult<()> {
        self.inner.mem_write(w, addr, val)
    }
    fn mem_read32(&mut self, addr: u32) -> BusResult<u32> {
        self.inner.mem_read32(addr)
    }
    fn mem_write_bytes(&mut self, addr: u32, data: &[u8]) -> BusResult<()> {
        self.inner.mem_write_bytes(addr, data)
    }
    fn mem_read_bytes(&mut self, addr: u32, out: &mut [u8]) -> BusResult<()> {
        self.inner.mem_read_bytes(addr, out)
    }
    fn now_ms(&mut self) -> u64 {
        self.inner.now_ms()
    }
    fn note(&mut self, text: &str) -> BusResult<()> {
        self.inner.note(text)
    }
    fn flush(&mut self) -> BusResult<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;
    use crate::bus::BusError;
    use crate::membus::MemBus;
    use crate::trace::{OpKind, TraceEntry};

    fn kinds(b: &MemBus) -> alloc::vec::Vec<(OpKind, u32)> {
        b.trace
            .iter()
            .filter_map(|e| match e {
                TraceEntry::Op(o) => Some((o.kind, o.a)),
                TraceEntry::Note(_) => None,
            })
            .collect()
    }

    #[test]
    fn dead_patterns() {
        for v in [u32::MAX, 0xDEAD_BEEF, 0xEAEA_EAEA] {
            assert!(looks_dead(v), "{v:#x}");
        }
        for v in [0, 1, 0xFFFF_FFFE, 0xDEAD_BEEE] {
            assert!(!looks_dead(v), "{v:#x}");
        }
    }

    #[test]
    fn chip_answers_needs_the_pci_id_and_a_live_sys_cfg1() {
        let mut b = MemBus::new();
        b.cfg.insert(0, RTL8852BE_PCI_ID);
        b.forced.insert(R_AX_SYS_CFG1, 0x1000);
        assert_eq!(chip_answers(&mut b), Ok(true));
        for v in [u32::MAX, RTW89_R32_DEAD, RTW89_R32_EA] {
            b.forced.insert(R_AX_SYS_CFG1, v);
            assert_eq!(chip_answers(&mut b), Ok(false), "{v:#x}");
        }
        b.forced.insert(R_AX_SYS_CFG1, 0x1000);
        b.cfg.insert(0, u32::MAX);
        assert_eq!(chip_answers(&mut b), Ok(false), "the function is gone");
        b.fail_at_op = Some(b.trace.len());
        assert_eq!(chip_answers(&mut b), Err(BusError::Gone));
    }

    #[test]
    fn errors_name_their_dead_read() {
        let e = Rtw89Error::MacNotEnabled {
            sel: "cmac",
            reg: R_AX_CMAC_FUNC_EN,
            value: RTW89_R32_DEAD,
        };
        assert_eq!(e.dead_read(), Some((R_AX_CMAC_FUNC_EN, RTW89_R32_DEAD)));
        let e = Rtw89Error::Bus(BusError::PollTimeout {
            addr: 0x1234,
            width: Width::W32,
            mask: 1,
            want: 0,
            last: u32::MAX,
        });
        assert_eq!(e.dead_read(), Some((0x1234, u32::MAX)));
        let live = Rtw89Error::C2hRegTimeout {
            reg: R_AX_C2HREG_CTRL,
            value: 0,
        };
        assert_eq!(live.dead_read(), None);
        assert_eq!(Rtw89Error::FwSecureBoot.dead_read(), None);
    }

    #[test]
    fn a_cmac_deadbeef_turns_the_clocks_on_and_reads_again() {
        let mut b = MemBus::new();
        b.poke(R_AX_CMAC_FUNC_EN, Width::W32, RTW89_R32_DEAD);
        let mut c = CmacRead::new(b);
        // a register that stays 0xDEADBEEF: ten CK_EN writes and reads, then Linux's value
        let v = c.read(Width::W32, R_AX_CMAC_FUNC_EN).unwrap();
        assert_eq!(v, RTW89_R32_DEAD, "never woke up: Linux returns DEAD");
        assert_eq!(
            (c.dead_reads, c.last_dead),
            (1, Some((R_AX_CMAC_FUNC_EN, RTW89_R32_DEAD)))
        );
        let mut want = vec![(OpKind::Rd, R_AX_CMAC_FUNC_EN)];
        for _ in 0..MAC_REG_POOL_COUNT {
            want.push((OpKind::Wr, R_AX_CK_EN));
            want.push((OpKind::Rd, R_AX_CMAC_FUNC_EN));
        }
        assert_eq!(kinds(&c.inner), want);
    }

    #[test]
    fn a_cmac_register_that_wakes_up_reads_its_value() {
        use crate::fw::hookbus::HookBus;
        let woke = core::cell::Cell::new(0u32);
        let mut h = HookBus::new(MemBus::new());
        h.on_read(|a| match a {
            R_AX_CMAC_FUNC_EN if woke.get() < 2 => {
                woke.set(woke.get() + 1);
                Some(RTW89_R32_DEAD)
            }
            R_AX_CMAC_FUNC_EN => Some(0x1234_5678),
            _ => None,
        });
        let mut c = CmacRead::new(h);
        assert_eq!(c.read(Width::W8, R_AX_CMAC_FUNC_EN + 1).unwrap(), 0x56);
        assert_eq!(c.dead_reads, 0);
        let ck = c
            .inner
            .mem
            .trace
            .iter()
            .filter(|e| matches!(e, TraceEntry::Op(o) if o.kind == OpKind::Wr && o.a == R_AX_CK_EN))
            .count();
        assert_eq!(ck, 2);
    }

    #[test]
    fn outside_the_cmac_nothing_changes() {
        let mut b = MemBus::new();
        b.poke(0x100, Width::W32, RTW89_R32_DEAD);
        let mut c = CmacRead::new(b);
        assert_eq!(c.read(Width::W32, 0x100).unwrap(), RTW89_R32_DEAD);
        c.rmw(Width::W32, 0x100, 0xff, 1).unwrap();
        c.poll(Width::W32, 0x100, 1, 1, 5).unwrap();
        assert_eq!(
            kinds(&c.inner),
            [
                (OpKind::Rd, 0x100),
                (OpKind::Rmw, 0x100),
                (OpKind::Poll, 0x100)
            ]
        );
    }

    #[test]
    fn cmac_rmw_and_poll_read_through_the_rule() {
        let mut b = MemBus::new();
        b.poke(R_AX_CK_EN + 0x10, Width::W32, 0xF0);
        let mut c = CmacRead::new(b);
        c.rmw(Width::W32, R_AX_CK_EN + 0x10, 0x30, 0x01).unwrap();
        assert_eq!(c.inner.peek(R_AX_CK_EN + 0x10, Width::W32), 0xC1);
        c.poll(Width::W32, R_AX_CK_EN + 0x10, 0xC0, 0xC0, 5)
            .unwrap();
        // a CMAC poll that never holds ends by the clock or the waited steps
        let r = c.poll(Width::W32, R_AX_CK_EN + 0x10, 0x2, 0x2, 3);
        assert!(
            matches!(r, Err(BusError::PollTimeout { last: 0xC1, .. })),
            "{r:?}"
        );
    }
}
