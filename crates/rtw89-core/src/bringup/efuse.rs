//! The physical efuse through the DDV engine, and the secure-boot bytes read
//! at the probe's power-on (JS kit/rtwpwr.mjs efuseDump, js/phy-efuse.mjs).

use alloc::vec::Vec;

use crate::bus::{Bus, BusError, Width, prep};
use crate::error::{Rtw89Error, Rtw89Result};
use crate::tables::regs::*;

/// efuse.c:15 EFUSE_EXTERNALPN_ADDR_AX (a define of efuse.c, so not in the generated headers)
pub const EFUSE_EXTERNALPN_ADDR_AX: u32 = 0x5EC;

// Linux: rtw89_dump_physical_efuse_map_ddv (efuse.c:113) with
// rtw89_enable_efuse_pwr_cut_ddv / _disable_ (efuse.c:74, :93); the bank switch
// (efuse.c:42) is 8852A only. A RDY poll that times out returns at once with
// the power cut still on, as Linux's -EBUSY. Linux keeps the byte from the
// poll's last read; an op poll returns none, so one read follows (golden trace).
fn dump_ddv<B: Bus + ?Sized>(bus: &mut B, addr: u32, size: u32, cv: u32) -> Rtw89Result<Vec<u8>> {
    bus.set(Width::W8, R_AX_PMC_DBG_CTRL2, B_AX_SYSON_DIS_PMCR_AX_WRMSK)?;
    bus.set(Width::W16, R_AX_SYS_ISO_CTRL, B_AX_PWC_EV2EF_B14)?;
    bus.delay_us(1000)?;
    bus.set(Width::W16, R_AX_SYS_ISO_CTRL, B_AX_PWC_EV2EF_B15)?;
    bus.clr(Width::W16, R_AX_SYS_ISO_CTRL, B_AX_ISO_EB2CORE)?;
    if cv == CHIP_CAV {
        bus.set32(R_AX_EFUSE_CTRL_1_V1, B_AX_EF_BURST)?;
    }
    let mut map = Vec::new();
    for a in addr..addr.saturating_add(size) {
        bus.w32(R_AX_EFUSE_CTRL, prep(B_AX_EF_ADDR_MASK, a) & !B_AX_EF_RDY)?;
        bus.poll32(R_AX_EFUSE_CTRL, B_AX_EF_RDY, B_AX_EF_RDY, 1000)?;
        map.push(bus.r32(R_AX_EFUSE_CTRL)? as u8);
    }
    if cv == CHIP_CAV {
        bus.clr32(R_AX_EFUSE_CTRL_1_V1, B_AX_EF_BURST)?;
    }
    bus.set(Width::W16, R_AX_SYS_ISO_CTRL, B_AX_ISO_EB2CORE)?;
    bus.clr(Width::W16, R_AX_SYS_ISO_CTRL, B_AX_PWC_EV2EF_B15)?;
    bus.delay_us(1000)?;
    bus.clr(Width::W16, R_AX_SYS_ISO_CTRL, B_AX_PWC_EV2EF_B14)?;
    bus.clr(Width::W8, R_AX_PMC_DBG_CTRL2, B_AX_SYSON_DIS_PMCR_AX_WRMSK)?;
    Ok(map)
}

// Linux: rtw89_dump_physical_efuse_map (efuse.c:211), dav false: five tries;
// only a timed-out RDY poll tries again, the last one's error is returned
pub fn dump_physical<B: Bus + ?Sized>(
    bus: &mut B,
    addr: u32,
    size: u32,
    cv: u32,
) -> Rtw89Result<Vec<u8>> {
    dump_physical_tries(bus, addr, size, cv, &mut |_, _| {})
}

/// [`dump_physical`] that tells `on_retry` (try from 0, its error) about every failed try,
/// Linux's "efuse dump (dav=0) failed, retrying" warning (B6: the caller keeps it)
pub fn dump_physical_tries<B: Bus + ?Sized>(
    bus: &mut B,
    addr: u32,
    size: u32,
    cv: u32,
    on_retry: &mut dyn FnMut(u32, &Rtw89Error),
) -> Rtw89Result<Vec<u8>> {
    bus.flush()?;
    let mut last = None;
    for retry in 0..5 {
        match dump_ddv(bus, addr, size, cv) {
            Err(e @ Rtw89Error::Bus(BusError::PollTimeout { .. })) => {
                on_retry(retry, &e);
                last = Some(e);
            }
            r => return r,
        }
    }
    Err(last.unwrap_or(Rtw89Error::Bus(BusError::Gone)))
}

// Linux: rtw89_efuse_read_fw_secure_ax (efuse.c:484): 2 bytes at 0x5EC. `None`
// when the dump failed (Linux warns and goes on). Like the JS driver, any pair
// other than FF/FF counts as secure boot; the MSS index/info recognition is not
// ported, the firmware loader (WL-2 fw) refuses secure boot instead.
pub fn read_fw_secure<B: Bus + ?Sized>(bus: &mut B, cv: u32) -> Rtw89Result<Option<bool>> {
    match dump_physical(bus, EFUSE_EXTERNALPN_ADDR_AX, 2, cv) {
        Ok(m) => Ok(Some(!(m.first() == Some(&0xff) && m.get(1) == Some(&0xff)))),
        Err(Rtw89Error::Bus(BusError::PollTimeout { .. })) => Ok(None),
        Err(e) => Err(e),
    }
}
