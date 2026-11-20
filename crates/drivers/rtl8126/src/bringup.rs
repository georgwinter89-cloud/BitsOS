//! The order, as data, and the steps of it that this crate can do. `Stage` also says who does what: the `Glue` stages
//! need memory, PCI or a kernel: `device::Card` does them, in this order.

use crate::chip::{self, Chip, ChipError};
use crate::hw::Hw;
use crate::regs::*;
use crate::seq::{self, PassedOver, SeqError, StartCtx};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum By {
    Core,
    Glue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    MapBar,
    Identify,
    ProbeSeq,
    ReadMac,
    SetMac,
    AllocRings,
    LoadFirmware,
    BusMaster,
    PhyUp,
    Cleanup,
    ArmRx,
    HwStart,
    Link,
    BusMasterOff,
    Stop,
}

impl Stage {
    pub const fn by(self) -> By {
        match self {
            Stage::MapBar
            | Stage::AllocRings
            | Stage::LoadFirmware
            | Stage::BusMaster
            | Stage::ArmRx
            | Stage::BusMasterOff => By::Glue,
            _ => By::Core,
        }
    }

    pub const fn linux(self) -> &'static str {
        match self {
            Stage::MapBar => "rtl_init_one M:5638-5653",
            Stage::Identify => "rtl_init_one M:5656-5660",
            Stage::ProbeSeq => "rtl_init_one M:5704-5710",
            Stage::ReadMac | Stage::SetMac => "rtl_init_mac_address M:5572-5596",
            Stage::AllocRings => "rtl_open M:5090-5102",
            Stage::LoadFirmware => "rtl_open, rtl_request_firmware M:2683",
            Stage::BusMaster => "rtl8169_up M:5034",
            Stage::PhyUp => "rtl8169_up M:5035-5037",
            Stage::Cleanup => "rtl_reset_work M:4310",
            Stage::ArmRx => "rtl_reset_work M:4312-4313",
            Stage::HwStart => "rtl_reset_work M:4316",
            Stage::Link => "phy_start M:5042, then the link interrupt or a look at BMSR",
            Stage::BusMasterOff => "rtl8169_down M:5017",
            Stage::Stop => "rtl8169_down M:5020-5021",
        }
    }
}

/// Probe, then open. The first rounds on the card can stop after any line (`plan/spec/gaming-pc.md` section 6): up to `SetMac` no
/// DMA exists, up to `PhyUp` no ring exists.
pub const BRING_UP: [Stage; 13] = [
    Stage::MapBar,
    Stage::Identify,
    Stage::ProbeSeq,
    Stage::ReadMac,
    Stage::SetMac,
    Stage::AllocRings,
    Stage::LoadFirmware,
    Stage::BusMaster,
    Stage::PhyUp,
    Stage::Cleanup,
    Stage::ArmRx,
    Stage::HwStart,
    Stage::Link,
];

/// Linux takes the bus master away before it stops the chip.
pub const TEAR_DOWN: [Stage; 2] = [Stage::BusMasterOff, Stage::Stop];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeError {
    Chip(ChipError),
    Seq(SeqError),
    /// Neither copy of the address is usable. Linux invents one here; a card that loses its address is worth a stop,
    /// and a made-up address on a home network helps nobody.
    NoMac,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Probe {
    pub chip: &'static Chip,
    pub mac: [u8; 6],
    /// CPlusCmd as found, for `StartCtx::new`.
    pub cp_cmd: u16,
}

/// `is_valid_ether_addr`: not all zero, not multicast.
pub const fn mac_usable(mac: &[u8; 6]) -> bool {
    let all_zero = mac[0] | mac[1] | mac[2] | mac[3] | mac[4] | mac[5] == 0;
    !all_zero && mac[0] & 1 == 0
}

fn mac_from<H: Hw + ?Sized>(hw: &H, base: u16) -> [u8; 6] {
    let mut mac = [0; 6];
    for (i, byte) in mac.iter_mut().enumerate() {
        *byte = hw.rd8(base + i as u16);
    }
    mac
}

/// `rtl_read_mac_address` for this family, then the running registers (M:5361-5362, :5583-5589).
pub fn read_mac<H: Hw + ?Sized>(hw: &H) -> Option<[u8; 6]> {
    [MAC0_BKP, MAC0].into_iter().map(|base| mac_from(hw, base)).find(mac_usable)
}

/// `rtl_rar_set` (M:2564-2578): high word first, each write committed, under the config unlock. It loads the running
/// filter; `MAC0_BKP` stays what it is.
pub fn set_mac<H: Hw + ?Sized>(hw: &H, mac: &[u8; 6]) {
    hw.wr8(CFG9346, CFG9346_UNLOCK);
    hw.wr32(MAC4, u32::from(u16::from_le_bytes([mac[4], mac[5]])));
    hw.rd8(CHIP_CMD);
    hw.wr32(MAC0, u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]));
    hw.rd8(CHIP_CMD);
    hw.wr8(CFG9346, CFG9346_LOCK);
}

/// `Identify` to `SetMac`: everything that needs neither memory nor the bus master. CPlusCmd is read before the
/// reset, as Linux does (M:5697).
pub fn probe<H: Hw + ?Sized>(hw: &H) -> Result<Probe, ProbeError> {
    let chip = chip::identify(hw).map_err(ProbeError::Chip)?;
    let cp_cmd = hw.rd16(CPLUS_CMD);
    seq::play(hw, seq::PROBE, None).map_err(ProbeError::Seq)?;
    let mac = read_mac(hw).ok_or(ProbeError::NoMac)?;
    set_mac(hw, &mac);
    Ok(Probe { chip, mac, cp_cmd })
}

/// `probe` for the driver: waits that run out are counted, not fatal (`seq::play_lenient`).
pub fn probe_lenient<H: Hw + ?Sized>(hw: &H) -> Result<(Probe, PassedOver), ProbeError> {
    let chip = chip::identify(hw).map_err(ProbeError::Chip)?;
    let cp_cmd = hw.rd16(CPLUS_CMD);
    let over = seq::play_lenient(hw, seq::PROBE, None).map_err(ProbeError::Seq)?;
    let mac = read_mac(hw).ok_or(ProbeError::NoMac)?;
    set_mac(hw, &mac);
    Ok((Probe { chip, mac, cp_cmd }, over))
}

/// `Cleanup` to `HwStart`. `arm_rx` hands every receive descriptor to the chip between the two, because the chip must
/// be in reset while that happens and must find them armed when it starts.
pub fn start<H: Hw + ?Sized>(hw: &H, ctx: &StartCtx, arm_rx: impl FnOnce()) -> Result<(), SeqError> {
    seq::play(hw, seq::CLEANUP, None)?;
    arm_rx();
    seq::play(hw, seq::HW_START, Some(ctx))
}

/// `start` for the driver: returns what it went on without (`seq::play_lenient`).
pub fn start_lenient<H: Hw + ?Sized>(hw: &H, ctx: &StartCtx, arm_rx: impl FnOnce()) -> Result<PassedOver, SeqError> {
    let mut over = seq::play_lenient(hw, seq::CLEANUP, None)?;
    arm_rx();
    over.add(seq::play_lenient(hw, seq::HW_START, Some(ctx))?);
    Ok(over)
}

/// Never gives up half way; returns how many steps failed.
pub fn stop<H: Hw + ?Sized>(hw: &H) -> u32 {
    seq::play_all(hw, seq::STOP)
}
