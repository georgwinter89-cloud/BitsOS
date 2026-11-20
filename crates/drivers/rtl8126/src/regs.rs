//! Register map of the RTL8126A, from `M:` (the line is on each value). Only what this crate uses is named, plus
//! the registers it must never write.

pub const VENDOR_REALTEK: u16 = 0x10ec; // M:246
pub const DEVICE_8126: u16 = 0x8126; // M:246

/// Linux maps the first memory BAR (M:5646-5651). On this card BAR0 is I/O ports, so that is BAR2 (64 KiB, read from
/// the Gaming-PC on 5.11.). BAR4 holds the MSI-X table.
pub const REG_BAR: u8 = 2;

pub const MAC0: u16 = 0x00; // M:257
pub const MAC4: u16 = 0x04; // M:258
pub const MAR0: u16 = 0x08; // M:259
pub const TX_DESC_LO: u16 = 0x20; // M:262
pub const TX_DESC_HI: u16 = 0x24; // M:263
pub const INT_CFG0: u16 = 0x34; // M:433
pub const CHIP_CMD: u16 = 0x37; // M:268
pub const INTR_MASK: u16 = 0x38; // M:436
pub const INTR_STATUS: u16 = 0x3c; // M:437
pub const TX_CONFIG: u16 = 0x40; // M:273
pub const RX_CONFIG: u16 = 0x44; // M:279
pub const CFG9346: u16 = 0x50; // M:291
pub const CONFIG1: u16 = 0x52; // M:293
pub const CONFIG3: u16 = 0x54; // M:297
pub const CONFIG5: u16 = 0x56; // M:299
pub const INT_CFG1: u16 = 0x7a; // M:438
pub const TX_POLL: u16 = 0x90; // M:441
pub const OCPDR: u16 = 0xb0; // M:409
pub const GPHY_OCP: u16 = 0xb8; // M:419
pub const MCU: u16 = 0xd3; // M:368
pub const RX_MAX_SIZE: u16 = 0xda; // M:302
pub const CPLUS_CMD: u16 = 0xe0; // M:303
pub const INTR_MITIGATE: u16 = 0xe2; // M:304
pub const RX_DESC_LO: u16 = 0xe4; // M:314
pub const RX_DESC_HI: u16 = 0xe8; // M:315
pub const MISC: u16 = 0xf0; // M:421
pub const MAC0_BKP: u16 = 0x19e0; // M:443
pub const RSS_CTRL: u16 = 0x4500; // M:444
pub const Q_NUM_CTRL: u16 = 0x4800; // M:445
pub const EEE_TXIDLE_TIMER: u16 = 0x6048; // M:446

/// Written by Linux without a name (M:3865, :3886, :3921).
pub const UNNAMED_382: u16 = 0x382;
pub const UNNAMED_D8: u16 = 0xd8;
pub const UNNAMED_1880: u16 = 0x1880;

/// Interrupt coalescing registers, cleared for this chip (M:4058-4061).
pub const COALESCE: core::ops::Range<u16> = 0xa00..0xa80;

// ChipCmd (M:478-481)
pub const STOP_REQ: u8 = 0x80;
pub const CMD_RESET: u8 = 0x10;
pub const CMD_RX_ENB: u8 = 0x08;
pub const CMD_TX_ENB: u8 = 0x04;

// Cfg9346 (M:490-491). Any other value talks to the EEPROM interface, so `seq` only has these two.
pub const CFG9346_LOCK: u8 = 0x00;
pub const CFG9346_UNLOCK: u8 = 0xc0;

// MCU (M:369-375)
pub const NOW_IS_OOB: u8 = 1 << 7;
pub const RXTX_EMPTY: u8 = (1 << 5) | (1 << 4);
pub const LINK_LIST_RDY: u8 = 1 << 1;

pub const RXTX_EMPTY_2_MASK: u16 = 0x0103; // M:2730-2731
pub const RXDV_GATED_EN: u32 = 1 << 19; // M:425
pub const INT_CFG0_CLKREQEN: u8 = 1 << 3; // M:435
pub const RDY_TO_L23: u8 = 1 << 1; // M:527
pub const ASPM_EN: u8 = 1 << 0; // M:540

// CPlusCmd (M:554-559). Normal_mode | RxVlan | RxChkSum | INTT_MASK.
pub const RX_CHK_SUM: u16 = 1 << 5;
pub const CPCMD_MASK: u16 = (1 << 13) | (1 << 6) | RX_CHK_SUM | 0b11;

// RxConfig and TxConfig
pub const RX_FETCH_DFLT: u32 = 8 << 27; // M:455
pub const RX_PAUSE_SLOT_ON: u32 = 1 << 11; // M:286
pub const RX_DMA_BURST: u32 = 7 << 8; // M:289
pub const RX_VLAN: u32 = (1 << 22) | (1 << 23); // M:451-453
pub const ACCEPT_BROADCAST: u32 = 0x08; // M:497
pub const ACCEPT_MULTICAST: u32 = 0x04; // M:498
pub const ACCEPT_MY_PHYS: u32 = 0x02; // M:499
pub const RX_ACCEPT_ERR_MASK: u32 = 0x30; // M:496
pub const RX_ACCEPT_OK_MASK: u32 = 0x0f; // M:501
pub const RX_ACCEPT_MASK: u32 = 0x3f; // M:502
pub const TX_CONFIG_VALUE: u32 = (7 << 8) | (3 << 24); // M:69-70, :505-506, :2788-2789; no AUTO_FIFO on this chip

/// Everything this crate may write, at the width it writes it. The tests' fake refuses anything else.
pub const WRITES: &[(u16, Width)] = &[
    (MAC0, Width::B4),
    (MAC4, Width::B4),
    (MAR0, Width::B4),
    (MAR0 + 4, Width::B4),
    (TX_DESC_LO, Width::B4),
    (TX_DESC_HI, Width::B4),
    (INT_CFG0, Width::B1),
    (CHIP_CMD, Width::B1),
    (INTR_MASK, Width::B4),
    (INTR_STATUS, Width::B4),
    (TX_CONFIG, Width::B4),
    (RX_CONFIG, Width::B4),
    (CFG9346, Width::B1),
    (CONFIG1, Width::B1),
    (CONFIG3, Width::B1),
    (CONFIG5, Width::B1),
    (INT_CFG1, Width::B2),
    (TX_POLL, Width::B2),
    (OCPDR, Width::B4),
    (GPHY_OCP, Width::B4),
    (UNNAMED_382, Width::B2),
    (UNNAMED_D8, Width::B1),
    (MCU, Width::B1),
    (RX_MAX_SIZE, Width::B2),
    (CPLUS_CMD, Width::B2),
    (RX_DESC_LO, Width::B4),
    (RX_DESC_HI, Width::B4),
    (MISC, Width::B4),
    (UNNAMED_1880, Width::B2),
    (RSS_CTRL, Width::B4),
    (Q_NUM_CTRL, Width::B2),
    (EEE_TXIDLE_TIMER, Width::B2),
];

/// Reachable from nothing in this crate. FLASH and the eFuse window are the persistent parts of the card; CSI, ERI,
/// EPHY and PHYAR are other chips' ways to the same places; MAC0_BKP is the chip's own copy of its address.
pub const NEVER_WRITTEN: &[u16] = &[
    0x30,     // FLASH, M:266
    0x60,     // PHYAR, M:300
    0x64,     // CSIDR, M:345
    0x68,     // CSIAR, M:346
    0x6f,     // PMCH, M:351
    0x70,     // ERIDR, M:391
    0x74,     // ERIAR, M:392
    0x80,     // EPHYAR, M:355
    0xb4,     // OCPAR, M:415
    0xd2,     // TWSI, M:367
    0xdc,     // EFUSEAR, M:376
    MAC0_BKP,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Width {
    B1,
    B2,
    B4,
}

impl Width {
    pub const fn bytes(self) -> u16 {
        match self {
            Width::B1 => 1,
            Width::B2 => 2,
            Width::B4 => 4,
        }
    }
}

pub fn write_allowed(off: u16, width: Width) -> bool {
    if COALESCE.contains(&off) {
        return width == Width::B4 && off.is_multiple_of(4);
    }
    WRITES.iter().any(|&(o, w)| o == off && w == width)
}

/// The window a mapping must cover. The highest offset used is the EEE timer, not `Q_NUM_CTRL`.
pub const MIN_BAR_SIZE: u32 = EEE_TXIDLE_TIMER as u32 + 2;
