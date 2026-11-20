//! Which chip this is. Anything but the two RTL8126A revisions is refused: the sequences in `seq` are only right for
//! `RTL_GIGA_MAC_VER_70`, and the same PCI id family covers chips that want other values.

use crate::hw::Hw;
use crate::regs;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chip {
    pub name: &'static str,
    pub xid: u16,
    pub firmware: &'static str,
}

/// M:109-111. The Gaming-PC's card is XID 64a (Linux's log line, 5.11.).
pub const RTL8126A_3: Chip = Chip { name: "RTL8126A", xid: 0x64a, firmware: "rtl_nic/rtl8126a-3.fw" };
pub const RTL8126A_2: Chip = Chip { name: "RTL8126A", xid: 0x649, firmware: "rtl_nic/rtl8126a-2.fw" };

const XID_MASK: u16 = 0x7cf; // M:110

/// The firmware the Gaming-PC's Linux loaded: `linux-firmware` 1:20261019-1, unpacked (read from the file, 5.11.).
/// For the tool that checks a file; the file itself never enters the repository.
pub struct FirmwareFact {
    pub name: &'static str,
    pub version: &'static str,
    pub bytes: usize,
    pub start: u32,
    pub actions: u32,
    pub sha256: &'static str,
}

pub const FIRMWARE_3: FirmwareFact = FirmwareFact {
    name: RTL8126A_3.firmware,
    version: "rtl8126a-3_0.0.5 08/30/24",
    bytes: 13_232,
    start: 112,
    actions: 3279,
    sha256: "18ffaf3b873d217bf780503a80f04fe2b03c088ba2d0c2a3d626145cb8e53b26",
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChipError {
    /// TxConfig read all ones: the card does not answer (M:5656-5658).
    Gone,
    /// An XID this crate has no sequence for.
    Unknown(u16),
}

/// M:5660: the XID as Linux takes it from TxConfig.
pub const fn xid_of(tx_config: u32) -> u16 {
    ((tx_config >> 20) & 0xfcf) as u16
}

pub fn from_xid(xid: u16) -> Result<&'static Chip, ChipError> {
    match xid & XID_MASK {
        0x64a => Ok(&RTL8126A_3),
        0x649 => Ok(&RTL8126A_2),
        _ => Err(ChipError::Unknown(xid)),
    }
}

pub fn from_tx_config(tx_config: u32) -> Result<&'static Chip, ChipError> {
    if tx_config == u32::MAX {
        return Err(ChipError::Gone);
    }
    from_xid(xid_of(tx_config))
}

pub fn identify<H: Hw + ?Sized>(hw: &H) -> Result<&'static Chip, ChipError> {
    from_tx_config(hw.rd32(regs::TX_CONFIG))
}
