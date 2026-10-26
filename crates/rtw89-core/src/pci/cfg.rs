//! Config space of the RTL8852BE: the chip's DBI window for the extended
//! config bytes, its MDIO port to the PCIe PHY, the reference clock
//! calibration, the PCIe capability and rtw89_pci_basic_cfg (JS js/pci.mjs).

use crate::bus::{Bus, Width, prep};
use crate::error::{Rtw89Error, Rtw89Result};
use crate::tables::regs::*;

use super::rings::PciRings;

/// include/uapi/linux/pci_regs.h
pub const PCI_STATUS_CAP_LIST: u32 = 0x10;
pub const PCI_CAPABILITY_LIST: u16 = 0x34;
pub const PCI_CAP_ID_EXP: u32 = 0x10;
pub const PCI_EXP_LNKCTL: u16 = 0x10;
pub const PCI_EXP_DEVCTL2: u16 = 0x28;
pub const PCI_EXP_DEVCTL2_COMP_TMOUT_DIS: u32 = 0x0010;
pub const PCI_EXP_DEVCTL2_LTR_EN: u32 = 0x0400;
/// Where the Mini-PC's chip has its PCIe capability (js/CONTRACTS.md, spec/wlan.md §2.1)
pub const EXP_CAP_MEASURED: u16 = 0x70;
/// __pci_find_next_cap_ttl's bound (PCI_FIND_CAP_TTL): a loop guard against a
/// capability list that points back at itself, not a count limit
const CAP_TTL: u32 = 48;

/// The config byte at `addr` (< 0x100) out of its dword: BitsOS reads config space by dwords
fn byte_of(dw: u32, addr: u16) -> u32 {
    (dw >> ((u32::from(addr) & 3) * 8)) & 0xff
}

// Linux: rtw89_dbi_read8 (pci.c:2256)
pub fn dbi_read8<B: Bus + ?Sized>(bus: &mut B, addr: u16) -> Rtw89Result<u8> {
    let addr = u32::from(addr);
    bus.w16(R_AX_DBI_FLAG, addr & B_AX_DBI_ADDR_MSK)?;
    bus.w8(R_AX_DBI_FLAG + 2, B_AX_DBI_RFLAG >> 16)?;
    bus.poll(Width::W8, R_AX_DBI_FLAG + 2, 0xff, 0, 1)?;
    Ok(bus.r8(R_AX_DBI_RDATA + (addr & 3))?)
}

// Linux: rtw89_dbi_write8 (pci.c:2233)
pub fn dbi_write8<B: Bus + ?Sized>(bus: &mut B, addr: u16, data: u8) -> Rtw89Result<()> {
    let addr = u32::from(addr);
    let lsb = addr & B_AX_DBI_2LSB;
    let wa = (addr & B_AX_DBI_ADDR_MSK) | prep(B_AX_DBI_WREN_MSK, 1 << lsb);
    bus.w8(R_AX_DBI_WDATA + lsb, data.into())?;
    bus.w16(R_AX_DBI_FLAG, wa)?;
    bus.w8(R_AX_DBI_FLAG + 2, B_AX_DBI_WFLAG >> 16)?;
    bus.poll(Width::W8, R_AX_DBI_FLAG + 2, 0xff, 0, 1)?;
    Ok(())
}

// Linux: rtw89_pci_read_config_byte (pci.c:2298): config space below 0x100
// directly (its dword), the chip's extended bytes through DBI, as the JS driver
// does (Linux tries pci_read_config_byte first and falls back to DBI)
pub fn cfg_read8<B: Bus + ?Sized>(bus: &mut B, addr: u16) -> Rtw89Result<u8> {
    if addr < 0x100 {
        return Ok(byte_of(bus.cfg_read(addr & !3)?, addr) as u8);
    }
    dbi_read8(bus, addr)
}

/// The first config byte of the capability area: the status registers in it
/// (DEVSTA, LNKSTA, PMCSR's PME_Status) are write-1-to-clear
const CFG_CAP_AREA: u16 = 0x40;
/// The command/status dword; its upper half is PCI_STATUS (write-1-to-clear bits)
const CFG_CMD_STATUS: u16 = 0x04;

// Linux: rtw89_pci_write_config_byte (pci.c:2280), the same split. Linux writes
// below 0x100 with pci_write_config_byte, a byte access; BitsOS's Bus has only
// dword config access, so the byte goes in by read-modify-write. That must not
// write back a status register's write-1-to-clear bits: in the command/status
// dword the status half is written 0 (a byte at 0x06/0x07 then clears exactly
// the bits it carries, as Linux's byte write does), and the capability area
// (0x40..0xFF) is refused. Linux's rtw89 only ever writes bytes from 0x100 up
// (DBI), as V2 does.
pub fn cfg_write8<B: Bus + ?Sized>(bus: &mut B, addr: u16, v: u8) -> Rtw89Result<()> {
    if addr < 0x100 {
        if addr >= CFG_CAP_AREA {
            return Err(Rtw89Error::Invalid {
                what: "config byte write into the capability area (no byte config access)",
                value: addr.into(),
            });
        }
        let at = addr & !3;
        let mut dw = bus.cfg_read(at)?;
        if at == CFG_CMD_STATUS {
            dw &= 0xffff;
        }
        let sh = (u32::from(addr) & 3) * 8;
        bus.cfg_write(at, (dw & !(0xff << sh)) | (u32::from(v) << sh))?;
        return Ok(());
    }
    dbi_write8(bus, addr, v)
}

// Linux: rtw89_pci_check_mdio (pci.c:2120)
fn check_mdio<B: Bus + ?Sized>(bus: &mut B, addr: u32, speed: u32, rw_bit: u32) -> Rtw89Result<()> {
    bus.w8(R_AX_MDIO_CFG, addr & 0x1f)?;
    let v = u32::from(bus.r16(R_AX_MDIO_CFG)?);
    let pg = match (speed == PCIE_PHY_GEN1, addr < 0x20) {
        (true, true) => MDIO_PG0_G1,
        (true, false) => MDIO_PG1_G1,
        (false, true) => MDIO_PG0_G2,
        (false, false) => MDIO_PG1_G2,
    };
    let v = ((v & !B_AX_MDIO_PHY_ADDR_MASK) | prep(B_AX_MDIO_PHY_ADDR_MASK, pg)) & 0xffff;
    bus.w16(R_AX_MDIO_CFG, v)?;
    bus.set(Width::W16, R_AX_MDIO_CFG, rw_bit)?;
    bus.poll(Width::W16, R_AX_MDIO_CFG, rw_bit, 0, 2)?;
    Ok(())
}

// Linux: rtw89_read16_mdio (pci.c:2152)
pub fn mdio_read16<B: Bus + ?Sized>(bus: &mut B, addr: u32, speed: u32) -> Rtw89Result<u16> {
    check_mdio(bus, addr, speed, B_AX_MDIO_RFLAG)?;
    Ok(bus.r16(R_AX_MDIO_RDATA)?)
}

// Linux: rtw89_write16_mdio (pci.c:2167)
pub fn mdio_write16<B: Bus + ?Sized>(
    bus: &mut B,
    addr: u32,
    data: u16,
    speed: u32,
) -> Rtw89Result<()> {
    bus.w16(R_AX_MDIO_WDATA, data.into())?;
    check_mdio(bus, addr, speed, B_AX_MDIO_WFLAG)
}

// Linux: rtw89_write16_mdio_mask (pci.c:2182)
pub fn mdio_mask16<B: Bus + ?Sized>(
    bus: &mut B,
    addr: u32,
    mask: u32,
    data: u32,
    speed: u32,
) -> Rtw89Result<()> {
    let v = u32::from(mdio_read16(bus, addr, speed)?);
    let v = (v & !mask) | prep(mask, data);
    mdio_write16(bus, addr, (v & 0xffff) as u16, speed)
}

// Linux: rtw89_pci_auto_refclk_cal (pci.c:2400) with autook_en false (the
// 8852BE's): the PHY rate picks the MDIO pages; L1 is held off around the
// calibration-enable bit, no new calibration runs. A failed MDIO access still
// puts the L1 bit back (Linux's `goto end`, pci.c:2536-2546), so a retry or the
// stop never runs with L1 off. The first error is returned; Linux lets the
// restore's result replace it (`ret =` at the end label), which turns an MDIO
// failure followed by a good restore into success: not taken over.
pub fn auto_refclk_cal<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    let rate = u32::from(cfg_read8(bus, RTW89_PCIE_PHY_RATE as u16)?) & RTW89_PCIE_PHY_RATE_MASK;
    let phy = match rate {
        1 => PCIE_PHY_GEN1,
        2 => PCIE_PHY_GEN2,
        _ => return Err(Rtw89Error::PciePhyRate { value: rate }),
    };
    let l1_ctrl = RTW89_PCIE_L1_CTRL as u16;
    let bdr = cfg_read8(bus, l1_ctrl)?;
    let l1 = u32::from(bdr) & RTW89_PCIE_BIT_L1 != 0;
    if l1 {
        cfg_write8(bus, l1_ctrl, bdr & !(RTW89_PCIE_BIT_L1 as u8))?;
    }
    let cal = (|| {
        let v = u32::from(mdio_read16(bus, RAC_CTRL_PPR_V1, phy)?);
        if v & B_AX_CALIB_EN != 0 {
            mdio_write16(bus, RAC_CTRL_PPR_V1, (v & !B_AX_CALIB_EN) as u16, phy)?;
        }
        // autook_en is false for the 8852BE: no new calibration
        Ok(())
    })();
    // end: L1 back to what it was
    let restore = if l1 {
        cfg_write8(bus, l1_ctrl, bdr)
    } else {
        Ok(())
    };
    cal.and(restore)
}

// Linux: pci_find_capability(pdev, PCI_CAP_ID_EXP) (drivers/pci/pci.c, outside
// WZ:linux/), which the PCI core runs at enumeration for pdev->pcie_cap; the
// pcie_capability_* calls work relative to it. Walked once, kept in
// `rings.exp_cap`. The JS driver also stops when the capability is not at the
// measured 0x70; V2 walks the list and takes it where it is (spec/wlan.md §3:
// "Walk the capability list"; the binding to 10EC:B852 picks the device). No
// PCIe capability at all is an error before any config write.
pub fn exp_cap<B: Bus + ?Sized>(bus: &mut B, rings: &mut PciRings) -> Rtw89Result<u16> {
    if let Some(cap) = rings.exp_cap {
        return Ok(cap);
    }
    let status = bus.cfg_read(0x04)? >> 16;
    let mut pos = 0u32;
    let mut first = 0u32;
    if status & PCI_STATUS_CAP_LIST != 0 {
        first = bus.cfg_read(PCI_CAPABILITY_LIST)? & 0xff;
        pos = first;
        let mut ttl = CAP_TTL;
        while ttl > 0 && pos >= 0x40 {
            pos &= !3;
            let ent = bus.cfg_read(pos as u16)? & 0xffff;
            let id = ent & 0xff;
            if id == 0xff {
                pos = 0;
                break;
            }
            if id == PCI_CAP_ID_EXP {
                break;
            }
            pos = ent >> 8;
            ttl -= 1;
        }
        if pos < 0x40 || ttl == 0 {
            pos = 0;
        }
    }
    if pos == 0 {
        return Err(Rtw89Error::NoPcieCapability { status, first });
    }
    let cap = pos as u16;
    rings.exp_cap = Some(cap);
    Ok(cap)
}

/// What rtw89_pci_basic_cfg found (for the log)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BasicCfg {
    pub devctl2: u32,
    pub lnkctl: u32,
}

// Linux: rtw89_pci_basic_cfg (pci.c:4609) with resume false, after
// rtw89_chip_info_setup at probe. 8852B: disable_eq and filter_out are 8852C
// only. rtw89_pci_cpl_timeout_cfg (pci.c:4384): PCIe completion timeout off in
// DEVCTL2 (pcie_capability_set_word; DEVSTA2 above it is reserved, written 0).
// rtw89_pci_link_cfg (pci.c:4286) / rtw89_pci_l1ss_cfg would switch the chip's
// CLKREQ/ASPM/L1SS machinery on when the host enabled them: link power saving,
// which BitsOS leaves off (ASPM off in LNKCTL, measured). Returns LNKCTL for the log.
pub fn basic_cfg<B: Bus + ?Sized>(bus: &mut B, rings: &mut PciRings) -> Rtw89Result<BasicCfg> {
    let cap = exp_cap(bus, rings)?;
    let devctl2 = bus.cfg_read(cap + PCI_EXP_DEVCTL2)? & 0xffff;
    bus.cfg_write(
        cap + PCI_EXP_DEVCTL2,
        devctl2 | PCI_EXP_DEVCTL2_COMP_TMOUT_DIS,
    )?;
    let lnkctl = bus.cfg_read(cap + PCI_EXP_LNKCTL)? & 0xffff;
    Ok(BasicCfg { devctl2, lnkctl })
}
