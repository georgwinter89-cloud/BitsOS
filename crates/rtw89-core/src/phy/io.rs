//! BB and RF register access of the RTL8852B (JS kit/phyio.mjs; Linux phy.c / phy.h):
//! BB registers at cr_base 0x10000, RF registers either direct (addresses with ADSEL,
//! bit 16, at rf_base_addr {0xe000, 0xf000}) or through the SWSI engine
//! (rtw89_phy_write_rf_v1 / rtw89_phy_read_rf_v1). dbcc is off on BitsOS, so the
//! `_idx` variants are the plain ones (PHY 0).

use crate::bus::{Bus, field, lowbit, prep};
use crate::error::{Rtw89Error, Rtw89Result};
use crate::tables::regs::*;

/// rtw89_phy_gen_ax cr_base: BB registers sit at MMIO 0x10000 + addr
pub const BB: u32 = 0x10000;
pub const RFREG_MASK: u32 = 0xfffff;
pub const MASKDWORD: u32 = 0xffff_ffff;
/// rtw89_phy_gen_ax rf_base_addr per path (the 8852B has two)
const RF_BASE: [u32; 2] = [0xe000, 0xf000];
/// RF address bit that selects the direct window (phy.h RTW89_RF_ADDR_ADSEL_MASK)
const ADSEL: u32 = 1 << 16;
/// The SWSI busy poll: 1 ms (kit phyio; Linux's rtw89_phy_check_swsi_busy waits 30 us)
const SWSI_POLL_MS: u16 = 1;

fn bb(addr: u32) -> u32 {
    BB.wrapping_add(addr)
}

// Linux: rtw89_phy_write32 (phy.h)
pub fn phy_w32<B: Bus + ?Sized>(bus: &mut B, addr: u32, v: u32) -> Rtw89Result<()> {
    bus.w32(bb(addr), v)?;
    Ok(())
}

// Linux: rtw89_phy_write32_mask / rtw89_phy_write32_idx (phy.c:2111) with PHY 0. A whole
// dword is written directly (kit phyio: one op, where Linux's rtw89_write32_mask reads first).
pub fn phy_mask32<B: Bus + ?Sized>(bus: &mut B, addr: u32, mask: u32, v: u32) -> Rtw89Result<()> {
    if mask == MASKDWORD {
        bus.w32(bb(addr), v)?;
    } else {
        bus.mask32(bb(addr), mask, v)?;
    }
    Ok(())
}

// Linux: rtw89_phy_write32_set
pub fn phy_set32<B: Bus + ?Sized>(bus: &mut B, addr: u32, bits: u32) -> Rtw89Result<()> {
    bus.set32(bb(addr), bits)?;
    Ok(())
}

// Linux: rtw89_phy_write32_clr
pub fn phy_clr32<B: Bus + ?Sized>(bus: &mut B, addr: u32, bits: u32) -> Rtw89Result<()> {
    bus.clr32(bb(addr), bits)?;
    Ok(())
}

// Linux: rtw89_phy_read32
pub fn phy_r32<B: Bus + ?Sized>(bus: &mut B, addr: u32) -> Rtw89Result<u32> {
    Ok(bus.r32(bb(addr))?)
}

// Linux: rtw89_phy_read32_mask / rtw89_phy_read32_idx with PHY 0
pub fn phy_r32_mask<B: Bus + ?Sized>(bus: &mut B, addr: u32, mask: u32) -> Rtw89Result<u32> {
    Ok(field(bus.r32(bb(addr))?, mask))
}

fn rf_direct(path: u32, addr: u32) -> Rtw89Result<u32> {
    let base = RF_BASE.get(path as usize).ok_or(Rtw89Error::Invalid {
        what: "RF path past the chip's two",
        value: path,
    })?;
    Ok(base + ((addr & 0xff) << 2))
}

// Linux: rtw89_phy_write_rf_v1 (phy.c:1190): direct for ADSEL addresses
// (rtw89_phy_write_rf, phy.c:1128, then a 1 us delay), else rtw89_phy_write_rf_a
// (phy.c:1153) through SWSI after the busy check.
pub fn rf_w<B: Bus + ?Sized>(
    bus: &mut B,
    path: u32,
    addr: u32,
    mask: u32,
    data: u32,
) -> Rtw89Result<()> {
    let mask = mask & RFREG_MASK;
    if addr & ADSEL != 0 {
        let direct = rf_direct(path, addr)?;
        phy_mask32(bus, direct, mask, data)?;
        bus.delay_us(1)?;
        return Ok(());
    }
    bus.poll32(
        bb(R_SWSI_V1),
        B_SWSI_W_BUSY_V1 | B_SWSI_R_BUSY_V1,
        0,
        SWSI_POLL_MS,
    )?;
    let mut data = data & RFREG_MASK;
    let mut en = 0;
    if mask != RFREG_MASK {
        en = 1;
        phy_mask32(bus, R_SWSI_BIT_MASK_V1, RFREG_MASK, mask)?;
        data = data.wrapping_shl(lowbit(mask)) & RFREG_MASK;
    }
    let val = prep(B_SWSI_DATA_BIT_MASK_EN_V1, en)
        | prep(B_SWSI_DATA_PATH_V1, path)
        | prep(B_SWSI_DATA_ADDR_V1, addr)
        | prep(B_SWSI_DATA_VAL_V1, data);
    phy_mask32(bus, R_SWSI_DATA_V1, MASKDWORD, val)
}

// Linux: rtw89_phy_read_rf_v1: direct for ADSEL addresses, else rtw89_phy_read_rf_a
// through SWSI (busy check, read address, 2 us, data-done poll, the data)
pub fn rf_r<B: Bus + ?Sized>(bus: &mut B, path: u32, addr: u32, mask: u32) -> Rtw89Result<u32> {
    let mask = mask & RFREG_MASK;
    if addr & ADSEL != 0 {
        let direct = rf_direct(path, addr)?;
        return phy_r32_mask(bus, direct, mask);
    }
    bus.poll32(
        bb(R_SWSI_V1),
        B_SWSI_W_BUSY_V1 | B_SWSI_R_BUSY_V1,
        0,
        SWSI_POLL_MS,
    )?;
    let val = prep(B_SWSI_READ_ADDR_PATH_V1, path) | prep(B_SWSI_READ_ADDR_ADDR_V1, addr);
    phy_mask32(bus, R_SWSI_READ_ADDR_V1, B_SWSI_READ_ADDR_V1, val)?;
    bus.delay_us(2)?;
    bus.poll32(
        bb(R_SWSI_V1),
        B_SWSI_R_DATA_DONE_V1,
        B_SWSI_R_DATA_DONE_V1,
        SWSI_POLL_MS,
    )?;
    phy_r32_mask(bus, R_SWSI_V1, mask)
}

/// rtw89_phy_write_reg3_tbl rows (struct rtw89_reg3_def): {addr, mask, data}
pub fn write_reg3_tbl<B: Bus + ?Sized>(
    bus: &mut B,
    rows: &[crate::tables::MaskWrite],
) -> Rtw89Result<()> {
    for r in rows {
        phy_mask32(bus, r.addr, r.mask, r.data)?;
    }
    Ok(())
}
