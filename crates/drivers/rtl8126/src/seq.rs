//! The register sequences of `RTL_GIGA_MAC_VER_70` as tables, in Linux's order, each block naming its original.
//! Tables rather than code so that a test can walk every value without a card, and so that a wrong value is a wrong
//! line in one place. The branches for other chips are gone; `chip` refuses those chips.
//!
//! Deviations from Linux, all on purpose:
//! - ASPM stays off in the chip: Linux's "enable" call takes its disable branch when the OS cannot manage ASPM
//!   (M:3124-3185), and that branch is what `HW_START` plays both times. The Gaming-PC's capture shows ASPM disabled.
//! - No RX checksum, no VLAN stripping (M:1839-1862 with those features off).
//! - The filter takes the "accept all multicasts" branch of `rtl_set_rx_mode` (M:2835-2873).
//! - Linux reaches the two config-space bytes through the CSI window when the bus has no extended config space
//!   (M:2902-2953). The CSI window stays unwritten here: `play` reports the failed access, `play_lenient` counts it
//!   and goes on. Both bytes only matter to a link that enters L0s or L1, and ASPM stays off.

use crate::hw::{wait, Hw};
use crate::ocp::{self, OcpError};
use crate::regs::*;

/// What `done` waits for. M:2671, :2723, :2728, :5366, :3856.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cond {
    ResetDone,
    RxTxEmpty,
    RxTxEmpty2,
    LinkListReady,
    OcpE00eClear,
}

impl Cond {
    pub fn done<H: Hw + ?Sized>(self, hw: &H) -> bool {
        match self {
            Cond::ResetDone => hw.rd8(CHIP_CMD) & CMD_RESET == 0,
            Cond::RxTxEmpty => hw.rd8(MCU) & RXTX_EMPTY == RXTX_EMPTY,
            Cond::RxTxEmpty2 => hw.rd16(INTR_MITIGATE) & RXTX_EMPTY_2_MASK == RXTX_EMPTY_2_MASK,
            Cond::LinkListReady => hw.rd8(MCU) & LINK_LIST_RDY != 0,
            Cond::OcpE00eClear => matches!(ocp::mac_read(hw, 0xe00e), Ok(v) if v & (1 << 13) == 0),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// A read for its side effect: `rtl_pci_commit` (M:830).
    Commit,
    Wr8(u16, u8),
    Wr16(u16, u16),
    Wr32(u16, u32),
    Mod8 { off: u16, clear: u8, set: u8 },
    Mod16 { off: u16, clear: u16 },
    Mod32 { off: u16, clear: u32, set: u32 },
    ZeroCoalesce,
    Ocp(u16, u16),
    /// `r8168_mac_ocp_modify(reg, mask, set)`.
    OcpMod(u16, u16, u16),
    Delay(u32),
    Wait { cond: Cond, us: u32, tries: u32 },
    CfgMod8 { off: u16, clear: u8 },
    CfgWr8(u16, u8),
    MaxReadRequest,
    /// The next three take their values from `StartCtx`.
    CpCmd,
    TxRing,
    RxRing,
}

use Step::*;

pub type Seq = &'static [&'static [Step]];

const RX_CONFIG_VALUE: u32 = RX_FETCH_DFLT | RX_DMA_BURST | RX_PAUSE_SLOT_ON; // rtl_init_rxcfg, M:2612-2614
pub const RX_MAX_SIZE_VALUE: u16 = crate::desc::RX_MAX_SIZE;
pub const MTU: u16 = 1500;
const EEE_TXIDLE_VALUE: u16 = MTU + 14 + 0x20; // M:2220

/// M:2606-2614.
pub const RX_INIT: &[Step] = &[Wr32(RX_CONFIG, RX_CONFIG_VALUE)];

/// `rtl8169_irq_mask_and_ack` (M:1669).
pub const MASK_ACK: &[Step] = &[Wr32(INTR_MASK, 0), Wr32(INTR_STATUS, u32::MAX), Commit];

/// `rtl_enable_rxdvgate` with `rtl_wait_txrx_fifo_empty` (M:2734-2763).
pub const RXDV_GATE_ON: &[Step] = &[
    Mod32 { off: MISC, clear: 0, set: RXDV_GATED_EN },
    Delay(2000),
    Mod8 { off: CHIP_CMD, clear: 0, set: STOP_REQ },
    Wait { cond: Cond::RxTxEmpty, us: 100, tries: 42 },
    Wait { cond: Cond::RxTxEmpty2, us: 100, tries: 42 },
];

/// `rtl_hw_init_8125` after the gate (M:5518-5529).
pub const HW_INIT_REST: &[Step] = &[
    Mod8 { off: CHIP_CMD, clear: CMD_TX_ENB | CMD_RX_ENB, set: 0 },
    Delay(1000),
    Mod8 { off: MCU, clear: NOW_IS_OOB, set: 0 },
    OcpMod(0xe8de, 1 << 14, 0),
    Wait { cond: Cond::LinkListReady, us: 100, tries: 42 },
    Ocp(0xc0aa, 0x07d0),
    Ocp(0xc0a6, 0x0150),
    Ocp(0xc01e, 0x5555),
    Wait { cond: Cond::LinkListReady, us: 100, tries: 42 },
];

/// `rtl_hw_reset` (M:2676-2681).
pub const HW_RESET: &[Step] = &[Wr8(CHIP_CMD, CMD_RESET), Wait { cond: Cond::ResetDone, us: 100, tries: 100 }];

/// The chip-touching tail of `rtl_init_one` up to the interrupt allocation (M:5704-5710).
pub const PROBE: Seq = &[RX_INIT, MASK_ACK, RXDV_GATE_ON, HW_INIT_REST, HW_RESET];

const START_HEAD: &[Step] = &[Wr8(CFG9346, CFG9346_UNLOCK)];

/// `rtl_hw_aspm_clkreq_enable(false)` (M:3164-3185).
const ASPM_OFF: &[Step] = &[
    OcpMod(0xe092, 0x00ff, 0),
    Mod8 { off: INT_CFG0, clear: INT_CFG0_CLKREQEN, set: 0 },
    Mod8 { off: CONFIG5, clear: ASPM_EN, set: 0 },
];

/// M:4109-4111, then `rtl_hw_start_8125` for this chip (M:4039-4071).
const START_8125: &[Step] = &[
    CpCmd,
    Wr16(EEE_TXIDLE_TIMER, EEE_TXIDLE_VALUE),
    Wr8(INT_CFG0, 0),
    ZeroCoalesce,
    Wr16(INT_CFG1, 0),
    OcpMod(0xea84, 0, 0b11),
];

/// `rtl_hw_start_8126a` (M:3975-3980): the two config-space values; 0x27 means L0 7 us, L1 16 us.
const START_8126A: &[Step] = &[CfgMod8 { off: 0x0890, clear: 0x01 }, CfgWr8(0x070f, 0x27)];

/// `rtl_hw_start_8125_common` (M:3861-3928), VER_70's branches. Ends by opening the RXDV gate.
pub const START_COMMON: &[Step] = &[
    Mod8 { off: CONFIG3, clear: RDY_TO_L23, set: 0 },
    Wr16(UNNAMED_382, 0x221b),
    Wr32(RSS_CTRL, 0),
    Wr16(Q_NUM_CTRL, 0),
    OcpMod(0xd40a, 0x0010, 0x0000),
    Mod8 { off: CONFIG1, clear: 0x10, set: 0 },
    Ocp(0xc140, 0xffff),
    Ocp(0xc142, 0xffff),
    OcpMod(0xd3e2, 0x0fff, 0x03a9),
    OcpMod(0xd3e4, 0x00ff, 0x0000),
    OcpMod(0xe860, 0x0000, 0x0080),
    OcpMod(0xeb58, 0x0001, 0x0000),
    Mod8 { off: UNNAMED_D8, clear: 0x02, set: 0 },
    OcpMod(0xe614, 0x0700, 0x0400),
    OcpMod(0xe63e, 0x0c30, 0x0020),
    OcpMod(0xc0b4, 0x0000, 0x000c),
    OcpMod(0xeb6a, 0x00ff, 0x0033),
    OcpMod(0xeb50, 0x03e0, 0x0040),
    OcpMod(0xe056, 0x00f0, 0x0000),
    OcpMod(0xe040, 0x1000, 0x0000),
    OcpMod(0xea1c, 0x0003, 0x0001),
    OcpMod(0xea1c, 0x0300, 0x0000),
    OcpMod(0xe0c0, 0x4f0f, 0x4403),
    OcpMod(0xe052, 0x0080, 0x0068),
    OcpMod(0xd430, 0x0fff, 0x047f),
    OcpMod(0xea1c, 0x0004, 0x0000),
    OcpMod(0xeb54, 0x0000, 0x0001),
    Delay(1),
    OcpMod(0xeb54, 0x0001, 0x0000),
    Mod16 { off: UNNAMED_1880, clear: 0x0030 },
    Ocp(0xe098, 0xc302),
    Wait { cond: Cond::OcpE00eClear, us: 1000, tries: 10 },
    Mod32 { off: MISC, clear: RXDV_GATED_EN, set: 0 },
];

/// `rtl_enable_exit_l1` (M:3016-3017).
const EXIT_L1_ON: &[Step] = &[OcpMod(0xc0ac, 0, 0x1f80)];

/// `rtl_hw_start` from the descriptor registers on (M:4122-4136). The empty unlock/lock pair is `rtl_jumbo_config`
/// for a chip with nothing to set; kept so the order stays Linux's.
const START_TAIL: &[Step] = &[
    Wr16(RX_MAX_SIZE, RX_MAX_SIZE_VALUE),
    TxRing,
    RxRing,
    Wr8(CFG9346, CFG9346_LOCK),
    Wr8(CFG9346, CFG9346_UNLOCK),
    Wr8(CFG9346, CFG9346_LOCK),
    MaxReadRequest,
    Commit,
    Wr8(CHIP_CMD, CMD_TX_ENB | CMD_RX_ENB),
    Wr32(RX_CONFIG, RX_CONFIG_VALUE),
    Wr32(TX_CONFIG, TX_CONFIG_VALUE),
    Mod32 { off: RX_CONFIG, clear: RX_ACCEPT_ERR_MASK | RX_VLAN, set: 0 },
    Wr32(MAR0 + 4, u32::MAX),
    Wr32(MAR0, u32::MAX),
    Mod32 {
        off: RX_CONFIG,
        clear: RX_ACCEPT_OK_MASK,
        set: ACCEPT_BROADCAST | ACCEPT_MY_PHYS | ACCEPT_MULTICAST,
    },
];

/// `rtl_hw_start` (M:4104-4137) without its last line, the interrupt enable: the first rounds are polled.
pub const HW_START: Seq =
    &[START_HEAD, ASPM_OFF, START_8125, START_8126A, START_COMMON, EXIT_L1_ON, ASPM_OFF, START_TAIL];

/// `rtl_irq_enable` (M:1661).
pub const IRQ_ON: &[Step] = &[Wr32(INTR_MASK, crate::irq::IRQ_MASK)];

const RX_CLOSE: &[Step] = &[Mod32 { off: RX_CONFIG, clear: RX_ACCEPT_MASK, set: 0 }]; // M:2708
const AFTER_GATE: &[Step] = &[Delay(2000)]; // M:4292

/// `rtl8169_cleanup` (M:4269-4304); also what precedes every start (`rtl_reset_work`, M:4306).
pub const CLEANUP: Seq = &[MASK_ACK, RX_CLOSE, RXDV_GATE_ON, AFTER_GATE, HW_RESET];

/// `rtl_disable_exit_l1` (M:3027-3028).
const EXIT_L1_OFF: &[Step] = &[OcpMod(0xc0ac, 0x1f80, 0)];

/// `rtl8169_down` after the bus master is off (M:5018-5022).
pub const STOP: Seq = &[MASK_ACK, RX_CLOSE, RXDV_GATE_ON, AFTER_GATE, HW_RESET, EXIT_L1_OFF];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeqError {
    Timeout(Cond),
    Ocp(OcpError),
    ConfigSpace(u16),
    ReadRequest,
    /// A table with ring addresses was played without them.
    NoStartContext,
}

impl From<OcpError> for SeqError {
    fn from(e: OcpError) -> Self {
        SeqError::Ocp(e)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingError {
    Zero,
    /// M:5090-5091: rings need 256-byte alignment.
    Misaligned(u64),
}

/// The values `HW_START` takes from outside. No IOMMU stands between this card and memory in BitsOS, so an address
/// that cannot be a ring is refused before it reaches the chip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StartCtx {
    cp_cmd: u16,
    tx_ring: u64,
    rx_ring: u64,
}

impl StartCtx {
    /// `read_cp_cmd` is CPlusCmd as read at probe (M:5697); RX checksum is turned off, the rest is kept.
    pub fn new(read_cp_cmd: u16, tx_ring: u64, rx_ring: u64) -> Result<StartCtx, RingError> {
        for ring in [tx_ring, rx_ring] {
            if ring == 0 {
                return Err(RingError::Zero);
            }
            if !ring.is_multiple_of(crate::desc::RING_ALIGN as u64) {
                return Err(RingError::Misaligned(ring));
            }
        }
        Ok(StartCtx { cp_cmd: read_cp_cmd & CPCMD_MASK & !RX_CHK_SUM, tx_ring, rx_ring })
    }
}

fn run_step<H: Hw + ?Sized>(hw: &H, step: Step, ctx: Option<&StartCtx>) -> Result<(), SeqError> {
    match step {
        Commit => {
            hw.rd8(CHIP_CMD);
        }
        Wr8(off, val) => hw.wr8(off, val),
        Wr16(off, val) => hw.wr16(off, val),
        Wr32(off, val) => hw.wr32(off, val),
        Mod8 { off, clear, set } => hw.wr8(off, (hw.rd8(off) & !clear) | set),
        Mod16 { off, clear } => hw.wr16(off, hw.rd16(off) & !clear),
        Mod32 { off, clear, set } => hw.wr32(off, (hw.rd32(off) & !clear) | set),
        ZeroCoalesce => {
            for off in COALESCE.step_by(4) {
                hw.wr32(off, 0);
            }
        }
        Ocp(reg, val) => ocp::mac_write(hw, reg as i32, val)?,
        OcpMod(reg, mask, set) => ocp::mac_modify(hw, reg as i32, mask, set)?,
        Delay(us) => hw.delay_us(us),
        Wait { cond, us, tries } => {
            if !wait(hw, |h| cond.done(h), true, us, tries) {
                return Err(SeqError::Timeout(cond));
            }
        }
        CfgMod8 { off, clear } => {
            let val = hw.cfg_rd8(off).ok_or(SeqError::ConfigSpace(off))?;
            if !hw.cfg_wr8(off, val & !clear) {
                return Err(SeqError::ConfigSpace(off));
            }
        }
        CfgWr8(off, val) => {
            if !hw.cfg_wr8(off, val) {
                return Err(SeqError::ConfigSpace(off));
            }
        }
        MaxReadRequest => {
            if !hw.set_max_read_request_4096() {
                return Err(SeqError::ReadRequest);
            }
        }
        CpCmd => hw.wr16(CPLUS_CMD, ctx.ok_or(SeqError::NoStartContext)?.cp_cmd),
        // High before low: M:2807-2811.
        TxRing => {
            let ring = ctx.ok_or(SeqError::NoStartContext)?.tx_ring;
            hw.wr32(TX_DESC_HI, (ring >> 32) as u32);
            hw.wr32(TX_DESC_LO, ring as u32);
        }
        RxRing => {
            let ring = ctx.ok_or(SeqError::NoStartContext)?.rx_ring;
            hw.wr32(RX_DESC_HI, (ring >> 32) as u32);
            hw.wr32(RX_DESC_LO, ring as u32);
        }
    }
    Ok(())
}

/// Plays a sequence and stops at the first failure. Linux logs a timed-out wait and goes on; on a card nobody has
/// driven from BitsOS yet, the first failure is the fact worth reporting.
pub fn play<H: Hw + ?Sized>(hw: &H, seq: &[&[Step]], ctx: Option<&StartCtx>) -> Result<(), SeqError> {
    for step in seq.iter().flat_map(|part| part.iter()) {
        run_step(hw, *step, ctx)?;
    }
    Ok(())
}

/// What `play_lenient` went on without.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PassedOver {
    /// Config-space bytes and the read-request size that could not be set.
    pub pci: u32,
    /// Waits that ran out, and the first of them.
    pub waits: u32,
    pub first_wait: Option<Cond>,
}

impl PassedOver {
    pub fn any(&self) -> bool {
        self.pci + self.waits > 0
    }
    pub fn add(&mut self, other: PassedOver) {
        self.pci += other.pci;
        self.waits += other.waits;
        self.first_wait = self.first_wait.or(other.first_wait);
    }
}

/// As `play`, for the driver. Passed over and counted instead of ending the sequence:
/// - a config-space byte or the read-request size that cannot be set (module text);
/// - a wait for the FIFOs, the link list or OCP 0xe00e that ran out. Linux logs such a wait and carries on
///   (`rtl_loop_wait`, M:898-913), and in two weeks of this machine's journal it never logged one; so on the card this
///   is written for the wait passes, and where it does not, doing as Linux does is the better chance of a network.
/// A reset that does not end, a refused OCP address and a missing start context still end it.
pub fn play_lenient<H: Hw + ?Sized>(hw: &H, seq: &[&[Step]], ctx: Option<&StartCtx>) -> Result<PassedOver, SeqError> {
    let mut over = PassedOver::default();
    for step in seq.iter().flat_map(|part| part.iter()) {
        match run_step(hw, *step, ctx) {
            Ok(()) => {}
            Err(SeqError::ConfigSpace(_) | SeqError::ReadRequest) => over.pci += 1,
            Err(SeqError::Timeout(cond)) if cond != Cond::ResetDone => {
                over.waits += 1;
                over.first_wait = over.first_wait.or(Some(cond));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(over)
}

/// Plays all of it whatever fails and returns how many steps failed: stopping a card must not give up half way.
pub fn play_all<H: Hw + ?Sized>(hw: &H, seq: &[&[Step]]) -> u32 {
    let mut failed = 0;
    for step in seq.iter().flat_map(|part| part.iter()) {
        if run_step(hw, *step, None).is_err() {
            failed += 1;
        }
    }
    failed
}

/// `rtl8169_doorbell` (M:4551-4556): a 16-bit write, unlike the older chips' 8-bit TxPoll.
pub fn doorbell<H: Hw + ?Sized>(hw: &H) {
    hw.wr16(TX_POLL, 1);
}
