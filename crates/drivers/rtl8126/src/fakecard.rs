//! A register-level fake of the RTL8126A for host tests (NIC-10): byte-addressed registers with the chip's widths,
//! the two OCP windows, the PHY's pages behind them, XID, self-clearing resets, both rings as DMA on the driver's
//! own memory, link changes, one interrupt with the level-to-edge rule, a clock that waits advance.
//!
//! It refuses what Linux would not do, and remembers why (`violations`):
//! - a write outside `regs::WRITES`, to a register on `regs::NEVER_WRITTEN`, or of the wrong width; `Cfg9346` with
//!   anything but lock and unlock (every other value drives the EEPROM pins); Config and MAC registers while locked;
//! - a chip reset that the mask, the acknowledge, the RXDV gate and the stop request did not precede (`BEFORE_PROBE_RESET`
//!   or `BEFORE_RESET`), and a transmit/receive enable that not every write of `rtl_hw_start` preceded, in Linux's
//!   order (`BEFORE_ENABLE`). These lists are typed from `r8169_main.c` a second time, not taken from `seq`;
//! - an enable with a descriptor not armed, a ring register or `RxMaxSize` that does not fit the buffers, bus master
//!   off; any DMA outside memory the driver allocated.
//! After a refusal the chip does not start: no test passes by accident on a wrong order.

use crate::desc::{DESC_BYTES, DESC_OWN, FIRST_FRAG, LAST_FRAG, NUM_RX, NUM_TX, RING_END, RX_BUF_SIZE};
use crate::hw::Hw;
use crate::machine::{FwFile, IrqHook, IrqSetup, Machine, PciFail};
use crate::mem::{DmaMem, VecMem};
use crate::ocp::OCP_FLAG;
use crate::regs::*;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ksync::SpinLock;

/// Locally administered, not a real card's.
pub const MAC: [u8; 6] = [0x02, 0x81, 0x26, 0x00, 0x00, 0x01];
/// Autonegotiation takes this long after a restart.
pub const AN_DELAY_NS: u64 = 3_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    W8(u16, u8),
    W16(u16, u16),
    W32(u16, u32),
    Cfg(u16, u8),
    ReadRq,
    Enable(bool),
    PciOff,
    AspmOff,
    IrqSetup,
    /// The chip took a reset, and it started its transmitter and receiver.
    Reset,
    Started,
}

mod linux;
pub use linux::*;
use linux::ocp_reg;

#[derive(Clone)]
pub struct Knobs {
    pub tx_config: u32,
    pub phy_id: u32,
    pub sfp: bool,
    pub cable: bool,
    pub speed_bits: u16,
    /// Extended config space cannot be reached (and the read-request size cannot be set).
    pub no_cfg: bool,
    pub stuck_reset: bool,
    pub stuck_phy: bool,
    /// Each PHY access shows its result only at this look (0: at the first).
    pub slow_phy: u32,
    /// No DMA memory can be had.
    pub no_dma: bool,
    pub enable_fails: bool,
    pub enable_refused: bool,
    /// What `setup_irq` gives, and whether the interrupt then really arrives.
    pub irq: IrqSetup,
    pub irq_arrives: bool,
    /// Frames handed over are never finished.
    pub tx_stuck: bool,
    /// The chip zeroes the address field when it hands a receive descriptor back.
    pub scribble_addr: bool,
    /// The firmware file, and from when on it can be read.
    pub firmware: Option<Vec<u8>>,
    pub firmware_from_ns: u64,
    /// Where DMA memory starts.
    pub dma_base: u64,
}

impl Default for Knobs {
    fn default() -> Self {
        Knobs {
            tx_config: 0x64a0_0000,
            phy_id: 0x001c_c862,
            sfp: false,
            cable: true,
            speed_bits: 0x0020,
            no_cfg: false,
            stuck_reset: false,
            stuck_phy: false,
            slow_phy: 0,
            no_dma: false,
            enable_fails: false,
            enable_refused: false,
            irq: IrqSetup::MsiX,
            irq_arrives: true,
            tx_stuck: false,
            scribble_addr: false,
            firmware: None,
            firmware_from_ns: 0,
            dma_base: 0x1000_0000,
        }
    }
}

struct Inner {
    k: Knobs,
    mem: BTreeMap<u16, u8>,
    mac_ocp: BTreeMap<u32, u16>,
    phy_ocp: BTreeMap<u32, u16>,
    cfg: BTreeMap<u16, u8>,
    ops: Vec<Op>,
    /// Index into `ops` after the last reset, and after the last start.
    since_reset: usize,
    since_start: usize,
    resets: u32,
    /// The last reset was `rtl8169_cleanup`'s (the probe's alone does not allow a start).
    cleanup_reset: bool,
    violations: Vec<String>,
    now: u64,
    bus_master: bool,
    started: bool,
    tx_head: u32,
    rx_head: u32,
    sent: Vec<Vec<u8>>,
    dma: Vec<VecMem>,
    next_dma: u64,
    hook: Option<IrqHook>,
    irq_line: bool,
    /// When autonegotiation is done (`u64::MAX`: never started).
    an_done_at: u64,
    /// The link bit reads low once after the link was away.
    link_latched_low: bool,
    fw_reads: u32,
    /// The PHY window's value once the access in flight is done, and the looks until then.
    phy_pending: Option<(u32, u32)>,
}

pub struct Fake {
    s: SpinLock<Inner>,
    waits: crate::ocp::PhyWaits,
}

impl Fake {
    pub fn new() -> Fake {
        Fake::with(Knobs::default())
    }

    pub fn with(k: Knobs) -> Fake {
        let mut s = Inner {
            next_dma: k.dma_base,
            k,
            mem: BTreeMap::new(),
            mac_ocp: BTreeMap::new(),
            phy_ocp: BTreeMap::new(),
            cfg: BTreeMap::new(),
            ops: Vec::new(),
            since_reset: 0,
            since_start: 0,
            resets: 0,
            cleanup_reset: false,
            violations: Vec::new(),
            now: 1_000_000,
            bus_master: false,
            started: false,
            tx_head: 0,
            rx_head: 0,
            sent: Vec::new(),
            dma: Vec::new(),
            hook: None,
            irq_line: false,
            an_done_at: u64::MAX,
            link_latched_low: true,
            fw_reads: 0,
            phy_pending: None,
        };
        s.poke(MCU, &[RXTX_EMPTY | LINK_LIST_RDY | NOW_IS_OOB]);
        s.poke(INTR_MITIGATE, &0x0103u16.to_le_bytes());
        s.poke(CPLUS_CMD, &0x2061u16.to_le_bytes());
        s.poke(MAC0_BKP, &MAC);
        s.poke(MISC, &0u32.to_le_bytes());
        s.cfg.insert(0x890, 0xff);
        // What the PHY can do and what it advertises after reset: EEE for every mode, no multi-gigabit yet.
        for (reg, v) in [(0xa616, 0x6000), (0xa5c4, 0x0006), (0xa6ec, 0x0003), (0xa5d0, 0x0006), (0xa6d4, 0x0003), (0xa41e, 0x2000), (0xa408, 0x01e1)] {
            s.phy_ocp.insert(reg, v);
        }
        s.phy_ocp.insert(0xa400, 0x1800);
        Fake { s: SpinLock::new(s), waits: crate::ocp::PhyWaits::new() }
    }

    pub fn knobs(&self, f: impl FnOnce(&mut Knobs)) {
        f(&mut self.s.lock().k)
    }
    pub fn ops(&self) -> Vec<Op> {
        self.s.lock().ops.clone()
    }
    pub fn clear_ops(&self) {
        let mut s = self.s.lock();
        s.ops.clear();
        s.since_reset = 0;
        s.since_start = 0;
    }
    pub fn violations(&self) -> Vec<String> {
        self.s.lock().violations.clone()
    }
    pub fn started(&self) -> bool {
        self.s.lock().started
    }
    pub fn sent(&self) -> Vec<Vec<u8>> {
        core::mem::take(&mut self.s.lock().sent)
    }
    pub fn fw_reads(&self) -> u32 {
        self.s.lock().fw_reads
    }
    pub fn advance(&self, ns: u64) {
        self.s.lock().now += ns;
    }
    pub fn phy(&self, reg: u32) -> u16 {
        self.s.lock().phy_ocp.get(&reg).copied().unwrap_or(0)
    }
    pub fn mac_ocp(&self, reg: u32) -> u16 {
        self.s.lock().mac_ocp.get(&reg).copied().unwrap_or(0)
    }
    pub fn set_mac_ocp(&self, reg: u32, v: u16) {
        self.s.lock().mac_ocp.insert(reg, v);
    }
    pub fn reg32(&self, off: u16) -> u32 {
        self.s.lock().peek32(off)
    }
    /// Sets a register byte behind the driver's back.
    pub fn poke8(&self, off: u16, v: u8) {
        self.s.lock().poke(off, &[v]);
    }
    /// Position of the first op that `want` accepts; panics with the log when there is none.
    pub fn pos(&self, want: impl Fn(&Op) -> bool) -> usize {
        let ops = self.ops();
        ops.iter().position(want).unwrap_or_else(|| panic!("no such op in {ops:?}"))
    }
    pub fn pos_last(&self, want: impl Fn(&Op) -> bool) -> usize {
        let ops = self.ops();
        ops.iter().rposition(want).unwrap_or_else(|| panic!("no such op in {ops:?}"))
    }

    /// Pulls or plugs the cable; the chip reports the change.
    pub fn cable(&self, on: bool) {
        let hook = {
            let mut s = self.s.lock();
            s.k.cable = on;
            s.link_latched_low = true;
            if on {
                s.an_done_at = s.now + AN_DELAY_NS;
            }
            s.raise(crate::irq::LINK_CHG)
        };
        fire(hook);
    }

    /// A frame arrives from the wire. False when the chip had no way to take it.
    pub fn rx(&self, frame: &[u8]) -> bool {
        let (ok, hook) = {
            let mut s = self.s.lock();
            let ok = s.deliver(frame);
            (ok, if ok { s.raise(crate::irq::RX_OK) } else { None })
        };
        fire(hook);
        ok
    }
}

fn fire(hook: Option<IrqHook>) {
    if let Some(h) = hook {
        h(0);
    }
}

impl Inner {
    fn poke(&mut self, off: u16, bytes: &[u8]) {
        for (i, b) in bytes.iter().enumerate() {
            self.mem.insert(off + i as u16, *b);
        }
    }
    fn byte(&self, off: u16) -> u8 {
        self.mem.get(&off).copied().unwrap_or(0)
    }
    fn peek32(&self, off: u16) -> u32 {
        u32::from_le_bytes(core::array::from_fn(|i| self.byte(off + i as u16)))
    }
    fn refuse(&mut self, why: String) {
        self.violations.push(why);
    }

    fn link(&self) -> bool {
        self.k.cable && self.now >= self.an_done_at && self.phy_ocp.get(&0xa400).is_some_and(|b| b & 0x0800 == 0)
    }

    /// Sets a cause; returns the hook when the interrupt line rises and the message really goes out.
    fn raise(&mut self, cause: u32) -> Option<IrqHook> {
        let status = self.peek32(INTR_STATUS) | cause;
        self.poke(INTR_STATUS, &status.to_le_bytes());
        self.level()
    }

    fn level(&mut self) -> Option<IrqHook> {
        let high = self.peek32(INTR_STATUS) & self.peek32(INTR_MASK) != 0;
        let rose = high && !self.irq_line;
        self.irq_line = high;
        if rose && self.bus_master && self.k.irq_arrives && self.hook.is_some() {
            return self.hook.clone();
        }
        None
    }

    /// The driver's memory holding `len` bytes at `addr`.
    fn dma(&mut self, addr: u64, len: usize, what: &str) -> Option<(VecMem, usize)> {
        let hit = self.dma.iter().find(|m| addr >= m.addr && addr + len as u64 <= m.addr + m.size() as u64).cloned();
        if hit.is_none() {
            self.refuse(format!("DMA outside the driver's memory: {what} at {addr:#x}+{len:#x}"));
        }
        hit.map(|m| {
            let off = (addr - m.addr) as usize;
            (m, off)
        })
    }

    fn ring(&self, lo: u16, hi: u16) -> u64 {
        u64::from(self.peek32(lo)) | u64::from(self.peek32(hi)) << 32
    }

    fn desc(&mut self, base: u64, entry: u32, what: &str) -> Option<(VecMem, usize, u32, u64)> {
        let (m, off) = self.dma(base + u64::from(entry) * DESC_BYTES as u64, DESC_BYTES, what)?;
        let opts1 = m.rd32(off)?;
        let mut addr = [0u8; 8];
        m.copy_out(off + 8, &mut addr);
        Some((m, off, opts1, u64::from_le_bytes(addr)))
    }

    /// The log since the last reset, without the write that is being judged.
    fn before(&self) -> &[Op] {
        &self.ops[self.since_reset..self.ops.len() - 1]
    }

    fn reset(&mut self) {
        let cleanup = first_missing(BEFORE_RESET, self.before(), false);
        self.cleanup_reset = cleanup.is_none();
        if !self.cleanup_reset && first_missing(BEFORE_PROBE_RESET, self.before(), false).is_some() {
            self.refuse(format!("chip reset without what Linux does first: {cleanup:?}"));
        }
        self.ops.push(Op::Reset);
        self.since_reset = self.ops.len();
        self.resets += 1;
        self.started = false;
        self.tx_head = 0;
        self.rx_head = 0;
        // The chip's own defaults come back: a start that only modifies these does not end at Linux's values.
        self.poke(RX_CONFIG, &0x0100_0000u32.to_le_bytes());
        self.poke(TX_CONFIG, &0u32.to_le_bytes());
    }

    /// `ChipCmd = CmdTxEnb | CmdRxEnb`: the chip starts only when everything before it was done.
    fn enable(&mut self) {
        let before = self.violations.len();
        let no_cfg = self.k.no_cfg;
        if let Some(miss) = first_missing(BEFORE_ENABLE, self.before(), no_cfg) {
            self.refuse(format!("enable without, or before, Linux's {miss:?}"));
        }
        if self.resets == 0 || !self.cleanup_reset {
            self.refuse("enable without rtl8169_cleanup's reset before it".into());
        }
        if !self.bus_master {
            self.refuse("enable with bus master off".into());
        }
        if self.byte(CFG9346) != CFG9346_LOCK {
            self.refuse("enable with the config registers unlocked".into());
        }
        if self.peek32(MISC) & RXDV_GATED_EN != 0 {
            self.refuse("enable with the RXDV gate closed".into());
        }
        let (tx, rx) = (self.ring(TX_DESC_LO, TX_DESC_HI), self.ring(RX_DESC_LO, RX_DESC_HI));
        for i in 0..NUM_RX {
            let Some((_, _, opts1, addr)) = self.desc(rx, i, "RX descriptor") else { break };
            let want = DESC_OWN | if i == NUM_RX - 1 { RING_END } else { 0 } | RX_BUF_SIZE;
            if opts1 != want {
                self.refuse(format!("RX descriptor {i} is {opts1:#x}, an armed one is {want:#x}"));
                break;
            }
            if self.dma(addr, RX_BUF_SIZE as usize, "RX buffer").is_none() {
                break;
            }
        }
        for i in 0..NUM_TX {
            match self.desc(tx, i, "TX descriptor") {
                Some((_, _, 0, _)) => {}
                Some((_, _, opts1, _)) => {
                    self.refuse(format!("TX descriptor {i} is {opts1:#x} at the start, Linux clears the ring"));
                    break;
                }
                None => break,
            }
        }
        if self.violations.len() == before {
            self.started = true;
            self.ops.push(Op::Started);
            self.since_start = self.ops.len();
        }
    }

    fn passing(&self) -> bool {
        self.started && self.bus_master && first_missing(AFTER_ENABLE, &self.ops[self.since_start..], false).is_none()
    }

    /// The doorbell: every frame the driver gave is sent.
    fn transmit(&mut self) -> bool {
        if !self.passing() || self.k.tx_stuck || !self.link() {
            return false;
        }
        let base = self.ring(TX_DESC_LO, TX_DESC_HI);
        let mut any = false;
        for _ in 0..NUM_TX {
            let Some((m, off, opts1, addr)) = self.desc(base, self.tx_head, "TX descriptor") else { break };
            if opts1 & DESC_OWN == 0 {
                break;
            }
            let len = (opts1 & 0xffff) as usize;
            if opts1 & (FIRST_FRAG | LAST_FRAG) != FIRST_FRAG | LAST_FRAG || !(60..=1518).contains(&len) {
                self.refuse(format!("TX descriptor {} is {opts1:#x}: not one whole frame of 60 to 1518 bytes", self.tx_head));
                break;
            }
            let Some((buf, at)) = self.dma(addr, len, "TX buffer") else { break };
            let mut frame = alloc::vec![0u8; len];
            buf.copy_out(at, &mut frame);
            self.sent.push(frame);
            m.wr32(off, opts1 & !DESC_OWN);
            self.tx_head = if opts1 & RING_END != 0 { 0 } else { self.tx_head + 1 };
            any = true;
        }
        any
    }

    fn deliver(&mut self, frame: &[u8]) -> bool {
        if !self.passing() || !self.link() {
            return false;
        }
        let base = self.ring(RX_DESC_LO, RX_DESC_HI);
        let Some((m, off, opts1, addr)) = self.desc(base, self.rx_head, "RX descriptor") else { return false };
        let total = frame.len() + 4;
        if opts1 & DESC_OWN == 0 {
            return false;
        }
        if total > (opts1 & 0x3fff) as usize || total > usize::from(u16::from_le_bytes([self.byte(RX_MAX_SIZE), self.byte(RX_MAX_SIZE + 1)])) {
            return false;
        }
        let Some((buf, at)) = self.dma(addr, total, "RX buffer") else { return false };
        buf.copy_in(at, frame);
        buf.copy_in(at + frame.len(), &[0xfc; 4]);
        m.wr32(off + 4, 0);
        if self.k.scribble_addr {
            m.wr64(off + 8, 0);
        }
        m.wr32(off, (opts1 & RING_END) | FIRST_FRAG | LAST_FRAG | total as u32);
        self.rx_head = if opts1 & RING_END != 0 { 0 } else { self.rx_head + 1 };
        true
    }

    fn allowed(&mut self, off: u16, width: Width) {
        if NEVER_WRITTEN.contains(&off) {
            self.refuse(format!("write to a register the driver must never write: {off:#x}"));
        } else if !write_allowed(off, width) {
            self.refuse(format!("write outside the allow list: {off:#x} {width:?}"));
        }
        if [MAC0, MAC4, CONFIG1, CONFIG3, CONFIG5].contains(&off) && self.byte(CFG9346) != CFG9346_UNLOCK {
            self.refuse(format!("{off:#x} written while the config registers are locked"));
        }
        if self.started && [TX_DESC_LO, TX_DESC_HI, RX_DESC_LO, RX_DESC_HI, RX_MAX_SIZE].contains(&off) {
            self.refuse(format!("{off:#x} written while the chip runs"));
        }
    }

    fn phy_write(&mut self, reg: u32, mut data: u16) {
        if reg == 0xa400 {
            if data & 0x8000 != 0 && !self.k.stuck_reset {
                data &= !0x8000;
            }
            if data & 0x0200 != 0 {
                data &= !0x0200;
                self.an_done_at = self.now + AN_DELAY_NS;
                self.link_latched_low = true;
            }
        }
        self.phy_ocp.insert(reg, data);
    }

    /// The PHY window shows `busy` for `slow_phy` looks and `done` from then on.
    fn phy_answer(&mut self, done: u32, busy: u32) {
        let slow = self.k.slow_phy;
        self.phy_pending = (slow > 0).then_some((done, slow));
        self.poke(GPHY_OCP, &(if slow > 0 { busy } else { done }).to_le_bytes());
    }

    /// One look at the PHY window.
    fn phy_look(&mut self) {
        match self.phy_pending.take() {
            Some((done, 1)) => self.poke(GPHY_OCP, &done.to_le_bytes()),
            Some((done, left)) => self.phy_pending = Some((done, left - 1)),
            None => {}
        }
    }

    fn phy_read(&mut self, reg: u32) -> u16 {
        match reg {
            0xa402 => {
                let up = self.link();
                let done = self.k.cable && self.now >= self.an_done_at;
                let shown = up && !core::mem::replace(&mut self.link_latched_low, !up);
                0x7909 | if shown { 0x0004 } else { 0 } | if done { 0x0020 } else { 0 }
            }
            0xa404 => (self.k.phy_id >> 16) as u16,
            0xa406 => self.k.phy_id as u16,
            0xa434 => self.k.speed_bits | 0x0008,
            _ => self.phy_ocp.get(&reg).copied().unwrap_or(0),
        }
    }
}

impl Hw for Fake {
    fn rd8(&self, off: u16) -> u8 {
        self.s.lock().byte(off)
    }
    fn rd16(&self, off: u16) -> u16 {
        let s = self.s.lock();
        u16::from_le_bytes([s.byte(off), s.byte(off + 1)])
    }
    fn rd32(&self, off: u16) -> u32 {
        let mut s = self.s.lock();
        match off {
            TX_CONFIG if s.k.tx_config == u32::MAX => u32::MAX,
            TX_CONFIG => s.k.tx_config | (s.peek32(off) & 0x000f_ffff),
            GPHY_OCP => {
                let v = s.peek32(off);
                s.phy_look();
                v
            }
            _ => s.peek32(off),
        }
    }
    fn wr8(&self, off: u16, val: u8) {
        let mut s = self.s.lock();
        s.allowed(off, Width::B1);
        if off == CFG9346 && val != CFG9346_LOCK && val != CFG9346_UNLOCK {
            s.refuse(format!("Cfg9346 = {val:#x}: only lock and unlock exist, the rest is the EEPROM interface"));
            return;
        }
        s.ops.push(Op::W8(off, val));
        if off == CHIP_CMD {
            if val & CMD_RESET != 0 {
                s.reset();
                let left = if s.k.stuck_reset { CMD_RESET } else { 0 };
                s.poke(off, &[left]);
                return;
            }
            if val & (CMD_TX_ENB | CMD_RX_ENB) == CMD_TX_ENB | CMD_RX_ENB && !s.started {
                s.enable();
            }
            if val & (CMD_TX_ENB | CMD_RX_ENB) == 0 {
                s.started = false;
            }
        }
        s.poke(off, &[val]);
    }
    fn wr16(&self, off: u16, val: u16) {
        let hook = {
            let mut s = self.s.lock();
            s.allowed(off, Width::B2);
            s.ops.push(Op::W16(off, val));
            if off == TX_POLL {
                if s.transmit() { s.raise(crate::irq::TX_OK) } else { None }
            } else {
                s.poke(off, &val.to_le_bytes());
                None
            }
        };
        fire(hook);
    }
    fn wr32(&self, off: u16, val: u32) {
        let hook = {
            let mut s = self.s.lock();
            s.allowed(off, Width::B4);
            s.ops.push(Op::W32(off, val));
            match off {
                OCPDR => {
                    let reg = ocp_reg(val);
                    if val & OCP_FLAG != 0 {
                        s.mac_ocp.insert(reg, val as u16);
                    } else {
                        let data = match reg {
                            0xd006 if s.k.sfp => 0x0007,
                            _ => s.mac_ocp.get(&reg).copied().unwrap_or(0),
                        };
                        s.poke(OCPDR, &u32::from(data).to_le_bytes());
                    }
                    None
                }
                GPHY_OCP => {
                    let (reg, stuck) = (ocp_reg(val), s.k.stuck_phy);
                    if val & OCP_FLAG != 0 {
                        s.phy_write(reg, val as u16);
                        s.phy_answer(if stuck { OCP_FLAG } else { 0 }, OCP_FLAG);
                    } else {
                        let data = s.phy_read(reg);
                        s.phy_answer(u32::from(data) | if stuck { 0 } else { OCP_FLAG }, 0);
                    }
                    None
                }
                INTR_STATUS => {
                    let left = s.peek32(INTR_STATUS) & !val;
                    s.poke(INTR_STATUS, &left.to_le_bytes());
                    s.level()
                }
                INTR_MASK => {
                    if val != 0 && val != crate::irq::IRQ_MASK {
                        s.refuse(format!("IntrMask = {val:#x}: Linux writes 0 or {:#x}", crate::irq::IRQ_MASK));
                    }
                    s.poke(off, &val.to_le_bytes());
                    s.level()
                }
                _ => {
                    s.poke(off, &val.to_le_bytes());
                    None
                }
            }
        };
        fire(hook);
    }
    fn delay_us(&self, us: u32) {
        self.s.lock().now += u64::from(us) * 1000;
    }
    fn cfg_rd8(&self, off: u16) -> Option<u8> {
        let s = self.s.lock();
        if s.k.no_cfg { None } else { Some(s.cfg.get(&off).copied().unwrap_or(0)) }
    }
    fn cfg_wr8(&self, off: u16, val: u8) -> bool {
        let mut s = self.s.lock();
        if off != 0x890 && off != 0x70f {
            s.refuse(format!("config-space write outside the two bytes Linux writes: {off:#x}"));
            return false;
        }
        if s.k.no_cfg {
            return false;
        }
        s.cfg.insert(off, val);
        s.ops.push(Op::Cfg(off, val));
        true
    }
    fn set_max_read_request_4096(&self) -> bool {
        let mut s = self.s.lock();
        if s.k.no_cfg {
            return false;
        }
        s.ops.push(Op::ReadRq);
        true
    }
    fn phy_waits(&self) -> Option<&crate::ocp::PhyWaits> {
        Some(&self.waits)
    }
}

impl Machine for Fake {
    type Mem = VecMem;
    fn now_ns(&self) -> u64 {
        // Every look at the clock costs a little, so a loop that only watches the time ends.
        let mut s = self.s.lock();
        s.now += 1000;
        s.now
    }
    fn enable(&self, dma: bool) -> Result<(), PciFail> {
        let mut s = self.s.lock();
        if s.k.enable_fails || s.k.enable_refused {
            return Err(PciFail { refused: s.k.enable_refused, text: "enable_device: fake".into() });
        }
        s.ops.push(Op::Enable(dma));
        s.bus_master |= dma;
        Ok(())
    }
    fn pci_off(&self) {
        let mut s = self.s.lock();
        s.ops.push(Op::PciOff);
        s.bus_master = false;
        s.hook = None;
    }
    fn bus_master(&self) -> bool {
        self.s.lock().bus_master
    }
    fn aspm_off(&self) {
        self.s.lock().ops.push(Op::AspmOff);
    }
    fn restore_config(&self) -> usize {
        0
    }
    fn alloc_dma(&self, len: usize) -> Result<VecMem, String> {
        let mut s = self.s.lock();
        if s.k.no_dma {
            return Err(format!("DMA {len} bytes: fake has none"));
        }
        let m = VecMem::new(s.next_dma, len);
        s.next_dma += (len as u64).div_ceil(4096) * 4096 + 4096;
        s.dma.push(m.clone());
        Ok(m)
    }
    fn setup_irq(&self, hook: IrqHook) -> IrqSetup {
        let mut s = self.s.lock();
        s.ops.push(Op::IrqSetup);
        if s.k.irq == IrqSetup::Polled {
            return IrqSetup::Polled;
        }
        s.hook = Some(hook);
        s.irq_line = false;
        s.k.irq
    }
    fn irq_degraded(&self) -> bool {
        self.s.lock().hook.is_none()
    }
    fn firmware(&self, name: &str) -> FwFile {
        let mut s = self.s.lock();
        s.fw_reads += 1;
        match &s.k.firmware {
            Some(bytes) if s.now >= s.k.firmware_from_ns => FwFile::Found { path: format!("/lib/firmware/{name}"), bytes: bytes.clone() },
            _ => FwFile::Missing { why: format!("/lib/firmware/{name}: not there") },
        }
    }
}
