//! A register-level fake of the I226-V / 82576 for host tests: the registers the driver touches with the
//! semantics the spec and V1's tests describe (SWSM read-sets-SMBI, SWESMBI that sticks only when the firmware
//! lets go, SW_FW_SYNC firmware bits, MDIC with a PHY address, BMCR restart -> link after 500 ms, CTRL.RST,
//! GIO master, EECD auto-read only on the I226, RX/TX descriptor engines with the address-overwriting
//! write-back), a fake clock that `delay_us` advances, a PCI command register and a trace of what the driver
//! did in which order. Knobs set up the scenarios of V1's `nic-test.mjs`/`igb-test.mjs`.

use crate::hw::{cmd, Hw, PciFail};
use crate::mem::{DmaMem, VecMem};
use crate::regs::{self, ctrl, mdic, phy, status, swsm, Model};
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use ksync::SpinLock;

/// What the driver did, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ev {
    Wr(usize, u32),
    Pci(u16),
    MsiOff,
    AspmOff,
    EnableDma,
    Reset,
    PhyReset,
    /// The card wanted to DMA while bus master was off (the fake refuses it).
    DmaWithoutBme,
    /// Which reset bit the driver used (`Reset` follows).
    ResetBit(u32),
    /// BARs written back (with decode off).
    BarsRestored,
    /// `enable` found the card in D3hot and woke it (PC-2 `enable_device`).
    Woke,
}

pub const MAC: [u8; 6] = [0xC8, 0xFF, 0xBF, 0x06, 0x08, 0x34];
pub const AN_DELAY_NS: u64 = 500_000_000;

pub struct Knobs {
    pub cable: bool,
    /// SWSM.SMBI cannot be cleared at all.
    pub smbi_locked: bool,
    /// SWESMBI does not stick before this time (the firmware holds SWSM).
    pub fw_holds_swesmbi_until: u64,
    /// The firmware takes SWSM from this time on (SWESMBI no longer sticks; `u64::MAX` = never).
    pub fw_takes_swesmbi_at: u64,
    /// MDIC writes to the MMD data register fail (an MDIC error), as a PHY without that MMD register would.
    pub mmd_data_error: bool,
    /// SW_FW_SYNC bit 17 is cleared by the "firmware" at this time (`u64::MAX` = never).
    pub fw_releases_phy_at: u64,
    pub gio_stuck: bool,
    pub mdic_dead: bool,
    pub tx_stalled: bool,
    pub all_ones: bool,
    pub d3hot: bool,
    pub bars_lost: bool,
    /// `Hw::enable` answers with this error (PC-2's `enable_device` refused or failed).
    pub enable_fails: Option<PciFail>,
    pub queue_wont_enable: bool,
    /// MPC's next value (clear on read).
    pub missed: u32,
    /// The transmit engine runs whenever the driver reads TDH (a card sending in the background).
    pub tx_autorun: bool,
    /// What `setup_irqs` gives (NW-4; PC-3's `alloc_irqs` in the kernel).
    pub irq: crate::irq::IrqSetup,
    /// `irqs_degraded` answers this (a released set, a storm hold).
    pub irq_degraded: bool,
}

impl Default for Knobs {
    fn default() -> Self {
        Knobs {
            cable: true,
            smbi_locked: false,
            fw_holds_swesmbi_until: 0,
            fw_takes_swesmbi_at: u64::MAX,
            mmd_data_error: false,
            fw_releases_phy_at: 0,
            gio_stuck: false,
            mdic_dead: false,
            tx_stalled: false,
            all_ones: false,
            d3hot: false,
            bars_lost: false,
            enable_fails: None,
            queue_wont_enable: false,
            missed: 0,
            tx_autorun: false,
            irq: crate::irq::IrqSetup::Polled,
            irq_degraded: false,
        }
    }
}

struct St {
    regs: BTreeMap<usize, u32>,
    phy: [u16; 32],
    mmd_dev: u16,
    mmd_addr: u16,
    mmd_data_mode: bool,
    mmd_2500: u16,
    pci_cmd: u16,
    msi_on: bool,
    msix_on: bool,
    an_enabled: bool,
    link_at: Option<u64>,
    trace: Vec<Ev>,
    bus: Vec<VecMem>,
    next_addr: u64,
    sent: Vec<Vec<u8>>,
    k: Knobs,
}

pub struct Fake {
    pub model: Model,
    /// The hook the driver gave `setup_irqs` (tests fire it by hand).
    hook: SpinLock<Option<crate::hw::IrqHook>>,
    /// `setup_irqs` calls.
    pub irq_setups: AtomicU64,
    clock: AtomicU64,
    /// Waits through `pause_us` (where the kernel sleeps instead of spinning).
    pauses: AtomicU64,
    st: SpinLock<St>,
}

fn reg(s: &St, off: usize) -> u32 {
    s.regs.get(&off).copied().unwrap_or(0)
}

impl Fake {
    pub fn new(model: Model) -> Fake {
        let mut regs = BTreeMap::new();
        regs.insert(regs::STATUS, status::GIO_MASTER_ENABLE);
        regs.insert(regs::RAL, u32::from_le_bytes([MAC[0], MAC[1], MAC[2], MAC[3]]));
        regs.insert(regs::RAH, (1 << 31) | u16::from_le_bytes([MAC[4], MAC[5]]) as u32);
        regs.insert(regs::EECD, if model == Model::I226 { regs::EECD_AUTO_RD } else { 0 });
        regs.insert(regs::PHPM, regs::PHPM_FW_LOCKS | regs::PHPM_RST_COMPL);
        let mut phy = [0u16; 32];
        phy[phy::BMCR as usize] = phy::BMCR_POWER_DOWN;
        phy[phy::ANAR as usize] = 0x0C01;
        Fake {
            model,
            hook: SpinLock::new(None),
            irq_setups: AtomicU64::new(0),
            clock: AtomicU64::new(1_000_000_000),
            pauses: AtomicU64::new(0),
            st: SpinLock::new(St {
                regs,
                phy,
                mmd_dev: 0,
                mmd_addr: 0,
                mmd_data_mode: false,
                mmd_2500: 0,
                pci_cmd: cmd::MEMORY,
                msi_on: false,
                msix_on: false,
                an_enabled: false,
                link_at: None,
                trace: Vec::new(),
                bus: Vec::new(),
                next_addr: 0x1_0000_0000,
                sent: Vec::new(),
                k: Knobs::default(),
            }),
        }
    }

    pub fn knobs(&self, f: impl FnOnce(&mut Knobs)) {
        f(&mut self.st.lock().k);
    }
    /// Sets a register as a firmware or a previous kernel left it.
    pub fn poke(&self, off: usize, v: u32) {
        self.st.lock().regs.insert(off, v);
    }
    pub fn peek(&self, off: usize) -> u32 {
        reg(&self.st.lock(), off)
    }
    pub fn phy_reg(&self, r: u32) -> u16 {
        self.st.lock().phy[r as usize]
    }
    pub fn mmd_2500(&self) -> u16 {
        self.st.lock().mmd_2500
    }
    /// (MMD device in MMDAC, MMDAC in data mode): (0, false) = the window is back on clause-22 registers.
    pub fn mmd_window(&self) -> (u16, bool) {
        let s = self.st.lock();
        (s.mmd_dev, s.mmd_data_mode)
    }
    pub fn pci(&self) -> u16 {
        self.st.lock().pci_cmd
    }
    pub fn set_pci(&self, v: u16) {
        self.st.lock().pci_cmd = v;
    }
    pub fn set_msi(&self, msi: bool, msix: bool) {
        let mut s = self.st.lock();
        s.msi_on = msi;
        s.msix_on = msix;
    }
    pub fn msi(&self) -> (bool, bool) {
        let s = self.st.lock();
        (s.msi_on, s.msix_on)
    }
    pub fn trace(&self) -> Vec<Ev> {
        self.st.lock().trace.clone()
    }
    pub fn clear_trace(&self) {
        self.st.lock().trace.clear();
    }
    pub fn now(&self) -> u64 {
        self.clock.load(Ordering::Relaxed)
    }
    pub fn pauses(&self) -> u64 {
        self.pauses.load(Ordering::Relaxed)
    }
    pub fn advance(&self, ns: u64) {
        self.clock.fetch_add(ns, Ordering::Relaxed);
    }
    /// An interrupt on index `i` as PC-3 would deliver it (the driver's hook runs). False without a hook.
    pub fn fire(&self, i: u16) -> bool {
        let h = self.hook.lock().clone();
        h.map(|h| h(i)).is_some()
    }
    pub fn sent(&self) -> Vec<Vec<u8>> {
        core::mem::take(&mut self.st.lock().sent)
    }
    /// Cable in or out; a cable plugged into a PHY that autonegotiates gets link 500 ms later.
    pub fn set_cable(&self, on: bool) {
        let now = self.now();
        let mut s = self.st.lock();
        s.k.cable = on;
        s.link_at = (on && s.an_enabled).then_some(now + AN_DELAY_NS);
    }

    fn link_up(&self, s: &St) -> bool {
        s.k.cable && s.link_at.is_some_and(|t| self.now() >= t) && reg(s, regs::CTRL) & ctrl::SLU != 0
    }

    fn mac_reset(&self, s: &mut St) {
        s.trace.push(Ev::Reset);
        for r in [regs::RCTL, regs::TCTL, regs::TXDCTL, regs::RDBAL, regs::RDBAH, regs::RDLEN, regs::RDH, regs::RDT,
            regs::TDBAL, regs::TDBAH, regs::TDLEN, regs::TDH, regs::TDT, regs::EIMS, self.model.ims(), regs::IVAR0,
            regs::GPIE, regs::EITR0]
        {
            s.regs.insert(r, 0);
        }
        // The 82576's RXDCTL comes enabled out of reset (`V1:igcring.asm:215`): the driver must turn it off.
        s.regs.insert(regs::RXDCTL, if self.model == Model::I82576 { regs::QUEUE_ENABLE | regs::RX_WTHRESH1 } else { 0 });
        s.regs.insert(regs::CTRL, 0);
        let st = reg(s, regs::STATUS);
        s.regs.insert(regs::STATUS, st | status::GIO_MASTER_ENABLE);
        if self.model == Model::I226 {
            s.regs.insert(regs::EECD, regs::EECD_AUTO_RD);
        }
    }

    fn mdic(&self, s: &mut St, v: u32) -> u32 {
        if s.k.mdic_dead {
            return v & !mdic::READY;
        }
        let addr = (v >> mdic::PHY_SHIFT) & 0x1F;
        let r = (v >> mdic::REG_SHIFT) & 0x1F;
        let data = v as u16;
        let base = (v & !0xFFFF) | mdic::READY;
        if addr != self.model.phy_addr() {
            return base | mdic::ERROR;
        }
        let mmd = r == phy::MMDAC || r == phy::MMDAAD;
        if mmd && self.model != Model::I226 {
            return base | mdic::ERROR;
        }
        let an_ctrl = |s: &St| s.mmd_data_mode && s.mmd_dev == phy::MMD_AN && s.mmd_addr == phy::MMD_MULTIGBT_AN_CTRL;
        if v & mdic::OP_WRITE != 0 {
            if r == phy::MMDAC {
                s.mmd_dev = data & 0x1F;
                s.mmd_data_mode = data & phy::MMDAC_FUNC_DATA != 0;
            } else if r == phy::MMDAAD {
                if s.k.mmd_data_error && s.mmd_data_mode {
                    return base | mdic::ERROR;
                }
                if an_ctrl(s) {
                    s.mmd_2500 = data;
                } else if !s.mmd_data_mode {
                    s.mmd_addr = data;
                }
            } else if r == phy::BMCR {
                s.phy[0] = data & !phy::BMCR_RESTART_AN;
                if data & phy::BMCR_AN_ENABLE != 0 && data & phy::BMCR_POWER_DOWN == 0 {
                    s.an_enabled = true;
                    if data & phy::BMCR_RESTART_AN != 0 {
                        s.link_at = s.k.cable.then_some(self.now() + AN_DELAY_NS);
                    }
                }
            } else {
                s.phy[r as usize] = data;
            }
            base
        } else {
            let d = if r == phy::MMDAAD && an_ctrl(s) {
                s.mmd_2500
            } else if r == phy::MMDAAD {
                s.mmd_addr
            } else {
                s.phy[r as usize]
            };
            base | d as u32
        }
    }

    /// One frame from the wire into the next RX descriptor. `status` overrides the written-back status/error
    /// dword (DD is always set), `len` the written-back length. False if the card had no descriptor (missed),
    /// receive is off or there is no link.
    pub fn deliver_with(&self, frame: &[u8], status: Option<u32>, len: Option<u16>) -> bool {
        let mut s = self.st.lock();
        if reg(&s, regs::RCTL) & 2 == 0 || reg(&s, regs::RXDCTL) & regs::QUEUE_ENABLE == 0 || !self.link_up(&s) {
            return false;
        }
        let (head, tail) = (reg(&s, regs::RDH) as usize, reg(&s, regs::RDT) as usize);
        let count = reg(&s, regs::RDLEN) as usize / regs::DESC_SIZE;
        if count == 0 || head == tail {
            return false;
        }
        if s.pci_cmd & cmd::BUS_MASTER == 0 {
            s.trace.push(Ev::DmaWithoutBme);
            return false;
        }
        let base = (reg(&s, regs::RDBAH) as u64) << 32 | reg(&s, regs::RDBAL) as u64;
        let d = base + (head * regs::DESC_SIZE) as u64;
        let mut q = [0u8; 8];
        assert!(bus_read(&s.bus, d, &mut q), "RX descriptor {d:#x} outside the driver's DMA memory");
        let buf = u64::from_le_bytes(q);
        assert!(bus_write(&s.bus, buf, frame), "RX buffer {buf:#x} outside the driver's DMA memory");
        // Write-back: the first qword (where the address was) gets packet info, not the address.
        bus_write(&s.bus, d, &0xDEAD_BEEF_0000_0000u64.to_le_bytes());
        let st = status.unwrap_or(regs::RX_EOP) | regs::DESC_DD;
        bus_write(&s.bus, d + 8, &st.to_le_bytes());
        bus_write(&s.bus, d + 12, &len.unwrap_or(frame.len() as u16).to_le_bytes());
        s.regs.insert(regs::RDH, ((head + 1) % count) as u32);
        true
    }

    pub fn deliver(&self, frame: &[u8]) -> bool {
        self.deliver_with(frame, None, None)
    }

    /// The transmit engine: sends every descriptor between head and tail unless stalled, without link (the I226
    /// holds them), transmit off or bus master off. Returns how many it sent.
    pub fn run_tx(&self) -> usize {
        let mut s = self.st.lock();
        self.run_tx_locked(&mut s)
    }

    fn run_tx_locked(&self, s: &mut St) -> usize {
        if s.k.tx_stalled || !self.link_up(s) || reg(s, regs::TCTL) & regs::TCTL_EN == 0
            || reg(s, regs::TXDCTL) & regs::QUEUE_ENABLE == 0
        {
            return 0;
        }
        let count = reg(s, regs::TDLEN) as usize / regs::DESC_SIZE;
        let base = (reg(s, regs::TDBAH) as u64) << 32 | reg(s, regs::TDBAL) as u64;
        let mut n = 0;
        while count > 0 && reg(s, regs::TDH) != reg(s, regs::TDT) {
            if s.pci_cmd & cmd::BUS_MASTER == 0 {
                s.trace.push(Ev::DmaWithoutBme);
                break;
            }
            let head = reg(s, regs::TDH) as usize;
            let d = base + (head * regs::DESC_SIZE) as u64;
            let mut q = [0u8; 16];
            assert!(bus_read(&s.bus, d, &mut q), "TX descriptor outside the driver's DMA memory");
            let addr = u64::from_le_bytes(q[0..8].try_into().unwrap());
            let cmdw = u32::from_le_bytes(q[8..12].try_into().unwrap());
            let olinfo = u32::from_le_bytes(q[12..16].try_into().unwrap());
            assert_eq!(cmdw & 0xFFFF_0000, regs::TX_CMD_DATA, "descriptor {head}: cmd_type_len {cmdw:#x}");
            let len = (cmdw & 0xFFFF) as usize;
            assert_eq!(olinfo >> regs::TX_PAYLEN_SHIFT, len as u32, "PAYLEN");
            assert!(len >= regs::TX_MIN_LEN, "the MAC refuses frames below 17 bytes ({len})");
            let mut f = alloc::vec![0u8; len];
            assert!(bus_read(&s.bus, addr, &mut f), "TX buffer {addr:#x} outside the driver's DMA memory");
            s.sent.push(f);
            bus_write(&s.bus, d, &0xFEED_0000_0000_0000u64.to_le_bytes());
            bus_write(&s.bus, d + 12, &(olinfo | regs::DESC_DD).to_le_bytes());
            s.regs.insert(regs::TDH, ((head + 1) % count) as u32);
            n += 1;
        }
        n
    }
}

fn bus_find(bus: &[VecMem], a: u64, len: usize) -> Option<(&VecMem, usize)> {
    bus.iter().find_map(|m| {
        let off = a.checked_sub(m.addr)? as usize;
        (off + len <= m.size()).then_some((m, off))
    })
}

fn bus_read(bus: &[VecMem], a: u64, out: &mut [u8]) -> bool {
    bus_find(bus, a, out.len()).is_some_and(|(m, off)| m.copy_out(off, out))
}

fn bus_write(bus: &[VecMem], a: u64, data: &[u8]) -> bool {
    bus_find(bus, a, data.len()).is_some_and(|(m, off)| m.copy_in(off, data))
}

impl Hw for Fake {
    type Mem = VecMem;
    fn rd(&self, off: usize) -> u32 {
        let mut s = self.st.lock();
        if s.k.all_ones {
            return !0;
        }
        let now = self.now();
        match off {
            regs::STATUS => {
                let mut v = reg(&s, regs::STATUS) & !(status::LU | status::FD | status::SPEED_1000);
                if reg(&s, regs::CTRL) & ctrl::GIO_MASTER_DISABLE != 0 && !s.k.gio_stuck {
                    v &= !status::GIO_MASTER_ENABLE;
                }
                if self.link_up(&s) {
                    v |= status::LU | status::FD | status::SPEED_1000;
                }
                v
            }
            regs::SWSM => {
                let v = reg(&s, regs::SWSM);
                s.regs.insert(regs::SWSM, v | swsm::SMBI);
                v
            }
            regs::SW_FW_SYNC => {
                let mut v = reg(&s, regs::SW_FW_SYNC);
                if now >= s.k.fw_releases_phy_at {
                    v &= !regs::SWFW_PHY0_FW;
                    s.regs.insert(regs::SW_FW_SYNC, v);
                }
                v
            }
            regs::MPC => core::mem::take(&mut s.k.missed),
            regs::TDH if s.k.tx_autorun => {
                self.run_tx_locked(&mut s);
                reg(&s, regs::TDH)
            }
            r if r == self.model.icr() => {
                let v = reg(&s, r);
                s.regs.insert(r, 0);
                v
            }
            r => reg(&s, r),
        }
    }
    fn wr(&self, off: usize, v: u32) {
        let mut s = self.st.lock();
        s.trace.push(Ev::Wr(off, v));
        if s.k.all_ones {
            return;
        }
        let now = self.now();
        match off {
            regs::CTRL => {
                let old = reg(&s, regs::CTRL);
                if v & (ctrl::RST | ctrl::DEV_RST) != 0 {
                    s.trace.push(Ev::ResetBit(v & (ctrl::RST | ctrl::DEV_RST)));
                    self.mac_reset(&mut s);
                    return;
                }
                if v & ctrl::PHY_RST != 0 && old & ctrl::PHY_RST == 0 {
                    s.trace.push(Ev::PhyReset);
                    let p = reg(&s, regs::PHPM);
                    s.regs.insert(regs::PHPM, p & !regs::PHPM_RST_COMPL);
                    s.phy[0] = phy::BMCR_POWER_DOWN;
                    s.an_enabled = false;
                    s.link_at = None;
                }
                if v & ctrl::PHY_RST == 0 && old & ctrl::PHY_RST != 0 {
                    let p = reg(&s, regs::PHPM);
                    s.regs.insert(regs::PHPM, p | regs::PHPM_RST_COMPL);
                }
                s.regs.insert(regs::CTRL, v);
            }
            regs::MDIC => {
                let r = self.mdic(&mut s, v);
                s.regs.insert(regs::MDIC, r);
            }
            regs::SWSM => {
                let mut nv = v & (swsm::SMBI | swsm::SWESMBI);
                if s.k.smbi_locked {
                    nv |= swsm::SMBI;
                }
                if now < s.k.fw_holds_swesmbi_until || now >= s.k.fw_takes_swesmbi_at {
                    nv &= !swsm::SWESMBI;
                }
                s.regs.insert(regs::SWSM, nv);
            }
            regs::SW_FW_SYNC => {
                let cur = reg(&s, regs::SW_FW_SYNC);
                s.regs.insert(regs::SW_FW_SYNC, (v & 0xFFFF) | (cur & 0xFFFF_0000));
            }
            regs::RXDCTL | regs::TXDCTL if s.k.queue_wont_enable => {
                s.regs.insert(off, v & !regs::QUEUE_ENABLE);
            }
            regs::EICR => {
                let c = reg(&s, regs::EICR);
                s.regs.insert(regs::EICR, c & !v);
            }
            r => {
                s.regs.insert(r, v);
            }
        }
    }
    fn now_ns(&self) -> u64 {
        self.now()
    }
    fn delay_us(&self, us: u64) {
        self.advance(us.max(1) * 1000);
    }
    fn pause_us(&self, us: u64) {
        self.pauses.fetch_add(1, Ordering::Relaxed);
        self.advance(us.max(1) * 1000);
    }
    fn relax(&self) {
        self.advance(1000);
    }
    fn pci_command(&self) -> u16 {
        self.st.lock().pci_cmd
    }
    fn set_pci_command(&self, v: u16) {
        let mut s = self.st.lock();
        s.trace.push(Ev::Pci(v));
        s.pci_cmd = v;
    }
    fn enable(&self, dma: bool) -> Result<(), PciFail> {
        let mut s = self.st.lock();
        if let Some(e) = s.k.enable_fails.clone() {
            return Err(e);
        }
        if core::mem::take(&mut s.k.d3hot) {
            s.trace.push(Ev::Woke);
        }
        if dma {
            s.trace.push(Ev::EnableDma);
        }
        s.pci_cmd |= cmd::MEMORY | cmd::INTX_DISABLE | if dma { cmd::BUS_MASTER } else { 0 };
        let v = s.pci_cmd;
        s.trace.push(Ev::Pci(v));
        Ok(())
    }
    fn pci_off(&self) {
        let mut s = self.st.lock();
        s.trace.push(Ev::MsiOff);
        s.msi_on = false;
        s.msix_on = false;
        s.pci_cmd &= !cmd::BUS_MASTER;
        let v = s.pci_cmd;
        s.trace.push(Ev::Pci(v));
    }
    fn aspm_off(&self) {
        self.st.lock().trace.push(Ev::AspmOff);
    }
    fn restore_config(&self) -> usize {
        let mut s = self.st.lock();
        if s.k.bars_lost {
            s.trace.push(Ev::BarsRestored);
            s.k.bars_lost = false;
            s.k.all_ones = false;
            return 2;
        }
        0
    }
    fn alloc_dma(&self, len: usize) -> Result<VecMem, String> {
        let mut s = self.st.lock();
        let m = VecMem::new(s.next_addr, len);
        s.next_addr += (len as u64).div_ceil(4096) * 4096 + 4096;
        s.bus.push(m.clone());
        Ok(m)
    }
    fn setup_irqs(&self, _max: u16, hook: crate::hw::IrqHook) -> crate::irq::IrqSetup {
        self.irq_setups.fetch_add(1, Ordering::Relaxed);
        *self.hook.lock() = Some(hook);
        self.st.lock().k.irq
    }
    fn irqs_degraded(&self) -> bool {
        self.st.lock().k.irq_degraded
    }
}
