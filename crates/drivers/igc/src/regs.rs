//! Register map of the I226-V and the 82576 (QEMU's `igb`), transcribed from V1 with the line each value came
//! from (`V1:net/igc.asm:18-55`, `igcphy.asm:5-10`, `igcring.asm:1-9`, `igcirq.asm:21-32`) and checked against
//! Linux `igc_regs.h`/`igc_defines.h` and `igb`/QEMU `igb_regs.h`. Where the two kinds differ, `Model` says so.

/// The two kinds this driver knows. QEMU models only the 82576; the Mini-PC has two I226-V.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Model {
    I226,
    /// 82576 (QEMU `-device igb`): the I226's stand-in in tests.
    I82576,
}

pub const VENDOR_INTEL: u16 = 0x8086;
pub const DEV_I226_V: u16 = 0x125C;
pub const DEV_82576: u16 = 0x10C9;

impl Model {
    pub fn from_device(device: u16) -> Option<Model> {
        match device {
            DEV_I226_V => Some(Model::I226),
            DEV_82576 => Some(Model::I82576),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Model::I226 => "i226",
            Model::I82576 => "82576",
        }
    }
    /// Interrupt mask clear: moved on the I226 (`V1:igc.asm:42-45`).
    pub fn imc(self) -> usize {
        match self {
            Model::I226 => 0x150C,
            Model::I82576 => 0x00D8,
        }
    }
    /// Interrupt cause read (clears on read with GPIE.NSICR).
    pub fn icr(self) -> usize {
        match self {
            Model::I226 => 0x1500,
            Model::I82576 => 0x00C0,
        }
    }
    /// Interrupt mask set (`V1:igcirq.asm:21-22`).
    pub fn ims(self) -> usize {
        match self {
            Model::I226 => 0x1508,
            Model::I82576 => 0x00D0,
        }
    }
    /// MDIC PHY address: the I226's own PHY sits at 0, QEMU's 82576 at 1; any other address is an MDIC error
    /// there (`V1:igcphy.asm:48-52`).
    pub fn phy_addr(self) -> u32 {
        match self {
            Model::I226 => 0,
            Model::I82576 => 1,
        }
    }
}

// Registers on both kinds (`V1:igc.asm:18-41`).
pub const CTRL: usize = 0x0000;
pub const STATUS: usize = 0x0008;
pub const EECD: usize = 0x0010;
pub const CTRL_EXT: usize = 0x0018;
pub const MDIC: usize = 0x0020;
pub const RCTL: usize = 0x0100;
pub const TCTL: usize = 0x0400;
pub const GPIE: usize = 0x1514;
/// Extended interrupt cause set: a write raises the causes as if they happened (a software interrupt).
pub const EICS: usize = 0x1520;
pub const EIMS: usize = 0x1524;
pub const EIMC: usize = 0x1528;
pub const EICR: usize = 0x1580;
pub const EITR0: usize = 0x1680;
pub const IVAR0: usize = 0x1700;
pub const MTA: usize = 0x5200;
/// 128 dwords of multicast table.
pub const MTA_DWORDS: usize = 128;
pub const RAL: usize = 0x5400;
pub const RAH: usize = 0x5404;
pub const SWSM: usize = 0x5B50;
pub const SW_FW_SYNC: usize = 0x5B5C;
pub const RDBAL: usize = 0xC000;
pub const RDBAH: usize = 0xC004;
pub const RDLEN: usize = 0xC008;
pub const SRRCTL: usize = 0xC00C;
pub const RDH: usize = 0xC010;
pub const RDT: usize = 0xC018;
pub const RXDCTL: usize = 0xC028;
pub const TDBAL: usize = 0xE000;
pub const TDBAH: usize = 0xE004;
pub const TDLEN: usize = 0xE008;
pub const TDH: usize = 0xE010;
pub const TDT: usize = 0xE018;
pub const TXDCTL: usize = 0xE028;
// Statistics (clear on read, same offsets on both): CRC errors, missed packets (no room in the FIFO), receive
// no buffers (no descriptor). Linux `igc_update_stats`; the Mini-PC check list wants 0 missed.
pub const CRCERRS: usize = 0x4000;
pub const MPC: usize = 0x4010;
pub const RNBC: usize = 0x40A0;

// I226 only (`V1:igc.asm:46-55`).
pub const PHPM: usize = 0x0E14;
pub const EEER: usize = 0x0E30;
pub const IPCNFG: usize = 0x0E38;
pub const RXPBS: usize = 0x2404;
pub const TXPBS: usize = 0x3404;
pub const RFCTL: usize = 0x5008;
pub const MANC: usize = 0x5820;
pub const PTM_CTRL: usize = 0x12540;

pub mod ctrl {
    /// GIO master disable: no requests from the card across the reset (`V1:igc.asm:151`).
    pub const GIO_MASTER_DISABLE: u32 = 1 << 2;
    /// Set link up (`V1:igc.asm:82`).
    pub const SLU: u32 = 1 << 6;
    /// Force speed (bit 11) and force duplex (bit 12): cleared, autonegotiation decides (`V1:igc.asm:83`).
    pub const FORCE_SPEED_DUPLEX: u32 = 3 << 11;
    /// Global reset (spec, `V1:igc.asm:163`, proven on the Mini-PC).
    pub const RST: u32 = 1 << 26;
    /// Device reset of the I225/I226 (Linux `igc_reset_hw_base`, `IGC_CTRL_DEV_RST`): the fallback after
    /// failed restarts with `RST`.
    pub const DEV_RST: u32 = 1 << 29;
    pub const PHY_RST: u32 = 1 << 31;
}

pub mod status {
    pub const FD: u32 = 1 << 0;
    pub const LU: u32 = 1 << 1;
    pub const SPEED_100: u32 = 1 << 6;
    pub const SPEED_1000: u32 = 1 << 7;
    pub const GIO_MASTER_ENABLE: u32 = 1 << 19;
    /// I225/I226: together with SPEED_1000 (Linux `igc_get_speed_and_duplex_copper`).
    pub const SPEED_2500: u32 = 1 << 22;
}

/// NVM auto-read done (`V1:igc.asm:165`); QEMU never sets it.
pub const EECD_AUTO_RD: u32 = 1 << 9;
/// A driver owns the card; its firmware leaves it alone (`V1:igcring.asm:198`).
pub const CTRL_EXT_DRV_LOAD: u32 = 1 << 28;

pub mod mdic {
    pub const REG_SHIFT: u32 = 16;
    pub const PHY_SHIFT: u32 = 21;
    pub const OP_WRITE: u32 = 1 << 26;
    pub const OP_READ: u32 = 1 << 27;
    pub const READY: u32 = 1 << 28;
    pub const ERROR: u32 = 1 << 30;
}

pub mod swsm {
    /// Software's bit: the read that finds it clear sets it (`V1:igcphy.asm:8`).
    pub const SMBI: u32 = 1 << 0;
    /// Software's against the firmware's: sticks once the firmware lets go.
    pub const SWESMBI: u32 = 1 << 1;
}

/// SW_FW_SYNC PHY0: software's bit, the firmware's 16 bits above (`V1:igcphy.asm:10`).
pub const SWFW_PHY0_SW: u32 = 1 << 1;
pub const SWFW_PHY0_FW: u32 = 1 << 17;
/// MANC: PHY resets blocked by management (`V1:igcphy.asm:71`).
pub const MANC_BLK_PHY_RST: u32 = 1 << 18;
/// PHPM: go-link-down and "disable 1000 / 2500" bits a firmware may leave (`V1:igcphy.asm:17`).
pub const PHPM_FW_LOCKS: u32 = 0x860;
/// PHPM: PHY reset complete (`V1:igcphy.asm:85`).
pub const PHPM_RST_COMPL: u32 = 1 << 8;
/// PTM off: a PTM cycle across the reset hangs the I226 (`V1:igc.asm:149`).
pub const PTM_CTRL_KEEP: u32 = 0x3FFF_FFFF;

// I226 values after each reset (`V1:igc.asm:188-194`, Linux `igc_reset`/`igc_set_eee_i225`).
pub const RXPBS_I226: u32 = 0xA2;
pub const TXPBS_I226: u32 = 0x0400_0014;
/// RFCTL: IPv6 extension headers off (I225/I226 erratum).
pub const RFCTL_IPV6_EXT_DIS: u32 = 1 << 16;
pub const IPCNFG_EEE: u32 = 0x1C;
pub const EEER_EEE: u32 = 0x7_0000;

// Rings (`V1:igcring.asm:183-185, 214-234`).
pub const QUEUE_ENABLE: u32 = 1 << 25;
/// RXDCTL WTHRESH 1: each descriptor written back at once (matters while polling).
pub const RX_WTHRESH1: u32 = 1 << 16;
/// Advanced one-buffer descriptors, 2 KB buffers.
pub const SRRCTL_ADV_2K: u32 = 0x0200_0002;
/// RCTL: EN | BAM (broadcasts) | SECRC (strip CRC).
pub const RCTL_ON: u32 = 0x0400_8002;
pub const TCTL_EN: u32 = 1 << 1;
/// Pad short packets (to 60 bytes).
pub const TCTL_PSP: u32 = 1 << 3;
/// TX cmd_type_len: DEXT | data type | IFCS | EOP | RS, plus the length.
pub const TX_CMD_DATA: u32 = 0x2B30_0000;
/// TX olinfo: PAYLEN shift.
pub const TX_PAYLEN_SHIFT: u32 = 14;
pub const DESC_DD: u32 = 1 << 0;
pub const RX_EOP: u32 = 1 << 1;
/// Frame error in the RX write-back (`V1:igcring.asm:366`).
pub const RX_RXE: u32 = 1 << 31;

// Interrupts (`V1:igcirq.asm:21-32`), measured on the real I226 (spec net.md §2.2).
/// RX and TX queue 0 valid, both to EICR bit 0 (Linux `igc_assign_vector` for MSI).
pub const IVAR0_Q0_TO_0: u32 = 0x8080;
/// EICR bit 0 (queue 0) and bit 31 (the ICR causes).
pub const EIMS_Q0_OTHER: u32 = 0x8000_0001;
/// ICR cleared by every read.
pub const GPIE_NSICR: u32 = 1 << 0;
/// EITR interval: at most about 6000 interrupts a second (Linux's start value).
pub const EITR_START: u32 = 648;
/// TXDW, LSC, RXSEQ, RXDMT0, RXT0 (Linux igc `IMS_ENABLE_MASK`).
pub const IMS_CAUSES: u32 = 0x9D;

// MSI-X (NW-4), the same registers on the I226 and the 82576 (linux-6.6/drivers/net/ethernet/intel/igc/igc_regs.h:48-60,
// igc_defines.h:295-300; igb's e1000_regs.h has the same offsets for the 82576).
/// Extended interrupt auto clear: an EICR bit set here is cleared when its MSI-X message is sent.
pub const EIAC: usize = 0x152C;
/// Extended interrupt auto mask: with GPIE.EIAME, an EIMS bit set here is cleared when its message is sent (the
/// vector stays quiet until the driver writes EIMS again: NAPI).
pub const EIAM: usize = 0x1530;
/// Which vector the "other" causes (ICR: link change) go to: bits 15:8.
pub const IVAR_MISC: usize = 0x1740;
pub const IVAR_VALID: u32 = 0x80;
pub const GPIE_MSIX_MODE: u32 = 1 << 4;
pub const GPIE_EIAME: u32 = 1 << 30;
pub const GPIE_PBA: u32 = 1 << 31;
/// ICR link status change: the only "other" cause unmasked in MSI-X mode (Linux also takes DOUTSYNC and DRSTA,
/// which only count or reset; V2's health check covers both).
pub const ICR_LSC: u32 = 1 << 2;

/// EITR bit 31: the write does not reset the moderation counters (linux-6.6 igc_defines.h:295; `igc_write_itr`,
/// igc_main.c:5425, sets it on every write). NW-4 checker finding, applied by INT-A1.
pub const EITR_CNT_IGNR: u32 = 0x8000_0000;

/// EITR of vector `n`.
pub const fn eitr(n: u16) -> usize {
    EITR0 + 4 * n as usize
}

/// EITR's interval field (bits 14:2) for a gap of `us` microseconds between interrupts.
pub const fn eitr_interval(us: u32) -> u32 {
    (us << 2) & 0x7FFC
}

/// Default gap on the queue vector: 49 µs, at most ~20,000 interrupts a second (Linux `IGC_20K_ITR` = 196,
/// linux-6.6/drivers/net/ethernet/intel/igc/igc.h:370: the low-latency end of its adaptive range). The first
/// interrupt after a quiet spell is not delayed, so a lone MCP request gets its interrupt at once; a burst is
/// batched into one interrupt per 49 µs, far below PC-3's storm limit (100 per ms).
pub const ITR_US_DEFAULT: u32 = 49;

/// PHY registers (IEEE 802.3 clause 22) and the I226's MMD window (Linux `igc_phy.c`).
pub mod phy {
    pub const BMCR: u32 = 0;
    pub const BMCR_RESTART_AN: u16 = 0x0200;
    pub const BMCR_POWER_DOWN: u16 = 0x0800;
    pub const BMCR_AN_ENABLE: u16 = 0x1000;
    pub const ANAR: u32 = 4;
    /// 10 half/full, 100 half/full (Linux `NWAY_AR_*_CAPS`).
    pub const ANAR_SPEEDS: u16 = 0x01E0;
    /// Pause bits: not advertised (see `phy::advertise`).
    pub const ANAR_PAUSE: u16 = 0x0C00;
    pub const ANAR_SELECTOR_MASK: u16 = 0x001F;
    pub const ANAR_SELECTOR_8023: u16 = 0x0001;
    pub const CTRL1000: u32 = 9;
    pub const CTRL1000_FD: u16 = 0x0200;
    pub const CTRL1000_HD: u16 = 0x0100;
    /// MMD access control / address-data (clause 45 over clause 22).
    pub const MMDAC: u32 = 13;
    pub const MMDAAD: u32 = 14;
    pub const MMDAC_FUNC_DATA: u16 = 0x4000;
    /// Auto-negotiation MMD (7), multi-gigabit control register 0x20, 2.5GBASE-T full duplex bit.
    pub const MMD_AN: u16 = 7;
    pub const MMD_MULTIGBT_AN_CTRL: u16 = 0x20;
    pub const MULTIGBT_2500_FD: u16 = 0x0080;
}

/// Descriptor and buffer geometry: 16-byte advanced descriptors, one 2 KB buffer per descriptor.
pub const DESC_SIZE: usize = 16;
pub const BUF_SIZE: usize = 2048;
/// The MAC refuses frames below 17 bytes even with padding on (Linux pads to 17 as well, `igcring.asm:318`).
pub const TX_MIN_LEN: usize = 17;
/// Largest frame taken from the wire: 1514 + a VLAN tag (CRC stripped).
pub const RX_MAX_FRAME: usize = 1518;
/// Largest frame accepted for sending (1514 + a VLAN tag).
pub const TX_MAX_FRAME: usize = 1518;
