//! TX power of the RTL8852B (JS js/chan-txpwr.mjs): by rate, offsets, limits and RU limits from
//! the firmware's tables (phy.c, AX generation), TX shape and the BB power reference
//! (rtw8852b_common.c). No SAR source, no antenna gain tables and no 6 GHz on BitsOS: those
//! terms take Linux's "none" values. Units: RF 1/4 dB, MAC 1/2 dB, BB 1/8 dB.

use crate::bus::{Bus, prep};
use crate::error::Rtw89Result;
use crate::events::EventLog;
use crate::phy::io::{MASKDWORD, phy_mask32};
use crate::tables::chan::{DFIR_FLAT, DFIR_SHARP, DFIR_SHARP14};
use crate::tables::regs::*;

use super::fw::{RateDesc, Regd, TxpwrTables};
use super::{ChanEvent, RtwChan};

/// rtw8852b.c:983-985 txpwr_factor_bb 3, _rf 2, _mac 1
const FACTOR_BB: u32 = 3;
const FACTOR_RF: u32 = 2;
const FACTOR_MAC: u32 = 1;
const S8_MAX: i32 = 127;
/// sar.h (not in linux/): RTW89_SAR_TXPWR_MAC_MAX is S8_MAX; it never wins against the TPE
/// constraint below (63), so its exact value does not matter here
const SAR_TXPWR_MAC_MAX: i32 = S8_MAX;

// Linux: rtw89_phy_txpwr_rf_to_mac (phy.h:1111)
fn rf_to_mac(v: i32) -> i32 {
    v >> (FACTOR_RF - FACTOR_MAC)
}
// Linux: rtw89_phy_txpwr_dbm_to_mac (phy.h:1118)
fn dbm_to_mac(dbm: i32) -> i32 {
    (dbm << FACTOR_MAC).clamp(-64, 63)
}
// Linux: rtw89_phy_txpwr_mac_to_bb (phy.h:1132)
fn mac_to_bb(v: i32) -> i32 {
    v << (FACTOR_BB - FACTOR_MAC)
}

// Linux: rtw89_mac_get_txpwr_cr_ax (mac.c:6258), PHY 0, so the address is the base. The 8852BE
// on PCIe runs quota mode SCC (the JS `d.mac.qtaMode`): CMAC1 addresses are refused.
fn txpwr_cr(base: u32, ev: &mut EventLog<ChanEvent>) -> Option<u32> {
    let scc = true;
    if !(R_AX_PWR_RATE_CTRL..=CMAC1_END_ADDR_AX).contains(&base)
        || (base >= CMAC1_START_ADDR_AX && scc)
    {
        ev.push(ChanEvent::TxpwrCrRefused { addr: base });
        return None;
    }
    Some(base)
}

// Linux: rtw89_mac_txpwr_write32 (mac.h:1588)
pub fn mac_txpwr_w32<B: Bus + ?Sized>(
    bus: &mut B,
    base: u32,
    v: u32,
    ev: &mut EventLog<ChanEvent>,
) -> Rtw89Result<()> {
    if let Some(cr) = txpwr_cr(base, ev) {
        bus.w32(cr, v)?;
    }
    Ok(())
}

// Linux: rtw89_mac_txpwr_write32_mask (mac.h:1602)
pub fn mac_txpwr_mask32<B: Bus + ?Sized>(
    bus: &mut B,
    base: u32,
    mask: u32,
    v: u32,
    ev: &mut EventLog<ChanEvent>,
) -> Rtw89Result<()> {
    if let Some(cr) = txpwr_cr(base, ev) {
        bus.mask32(cr, mask, v)?;
    }
    Ok(())
}

// Linux: rtw89_regd_get (core.h:8923): UK follows ETSI (regd.c:692, PCIe without ACPI DSM)
pub fn regd_get(regd: &Regd, band: u32) -> u32 {
    let r = regd
        .txpwr_regd
        .get(band as usize)
        .copied()
        .unwrap_or(RTW89_NA);
    if r == RTW89_UK { RTW89_ETSI } else { r }
}

// Linux: rtw89_channel_to_idx (phy.c:2535), 2 and 5 GHz
pub fn channel_to_idx(ch: u32) -> u32 {
    match ch {
        1..=14 => ch - 1,
        36..=64 => (ch - 36) >> 1,
        100..=144 => ((ch - 100) >> 1) + 15,
        149..=177 => ((ch - 149) >> 1) + 38,
        _ => 0,
    }
}

// Linux: rtw89_phy_read_txpwr_byrate (phy.c:2495): MAC units; a rate the table does not have
// reads 0
pub fn read_byrate(t: &TxpwrTables, band: u32, bw: u32, d: RateDesc) -> i32 {
    let band = if d.rs == RTW89_RS_CCK {
        RTW89_BAND_2G
    } else {
        band
    };
    let v = t.byr_head(band, bw).and_then(|h| h.seek(d)).unwrap_or(0);
    rf_to_mac(i32::from(v))
}

// the minimum of the limit with the SAR (none: RTW89_SAR_TXPWR_MAC_MAX, sar.c:288) and the TPE
// constraint (rtw89_phy_get_tpe_constraint, phy.c:2483: S8_MAX dBm outside 6 GHz)
fn cap_limit(lmt: i32) -> i32 {
    lmt.min(SAR_TXPWR_MAC_MAX).min(dbm_to_mac(S8_MAX))
}

// Linux: rtw89_phy_read_txpwr_limit (phy.c:2570), MAC units. No DA tables in this firmware, so
// rtw89_can_apply_ant_gain is false: no antenna gain offset. The regulation's 0 falls back to
// the worldwide value.
#[allow(clippy::too_many_arguments)]
pub fn read_txpwr_limit(
    t: &TxpwrTables,
    regd: &Regd,
    band: u32,
    bw: u32,
    ntx: u32,
    rs: u32,
    bf: u32,
    ch: u32,
) -> i32 {
    let tbl = if band == RTW89_BAND_2G {
        &t.lmt_2g
    } else {
        &t.lmt_5g
    };
    // rtw89_phy_validate_txpwr_limit_bw
    if bw as usize >= tbl.dims().first().copied().unwrap_or(0) {
        return 0;
    }
    let idx = channel_to_idx(ch) as usize;
    let at = |r: u32| {
        tbl.get(&[
            bw as usize,
            ntx as usize,
            rs as usize,
            bf as usize,
            r as usize,
            idx,
        ])
        .unwrap_or(0)
    };
    let mut lmt = at(regd_get(regd, band));
    if lmt == 0 {
        lmt = at(RTW89_WW);
    }
    cap_limit(rf_to_mac(i32::from(lmt).min(S8_MAX)))
}

// Linux: rtw89_phy_read_txpwr_limit_ru (phy.c:2855), MAC units
pub fn read_txpwr_limit_ru(
    t: &TxpwrTables,
    regd: &Regd,
    band: u32,
    ru: u32,
    ntx: u32,
    ch: u32,
) -> i32 {
    let tbl = if band == RTW89_BAND_2G {
        &t.lmt_ru_2g
    } else {
        &t.lmt_ru_5g
    };
    let idx = channel_to_idx(ch) as usize;
    let at = |r: u32| {
        tbl.get(&[ru as usize, ntx as usize, r as usize, idx])
            .unwrap_or(0)
    };
    let mut lmt = at(regd_get(regd, band));
    if lmt == 0 {
        lmt = at(RTW89_WW);
    }
    cap_limit(rf_to_mac(i32::from(lmt).min(S8_MAX)))
}

fn pack(v: &[i32; 4]) -> u32 {
    v.iter()
        .enumerate()
        .fold(0u32, |a, (i, &x)| a | ((x as u32 & 0xff) << (8 * i)))
}

// Linux: rtw89_phy_set_txpwr_byrate_ax (phy.c:3069) with rtw89_rs_idx_num_ax / rs_nss_num_ax:
// for the 8852B's 2 RF paths, nss 0 and 1; four rates per dword
fn set_byrate<B: Bus + ?Sized>(
    bus: &mut B,
    t: &TxpwrTables,
    chan: &RtwChan,
    ev: &mut EventLog<ChanEvent>,
) -> Rtw89Result<()> {
    let sections = [
        (RTW89_RS_CCK, RTW89_RATE_CCK_NUM, 1),
        (RTW89_RS_OFDM, RTW89_RATE_OFDM_NUM, 1),
        (RTW89_RS_MCS, RTW89_RATE_MCS_NUM_AX, RTW89_NSS_NUM),
        (RTW89_RS_HEDCM, RTW89_RATE_HEDCM_NUM, RTW89_NSS_HEDCM_NUM),
    ];
    let mut addr = R_AX_PWR_BY_RATE;
    let mut v = [0i32; 4];
    for nss in 0..2 {
        for &(rs, idx_num, nss_num) in &sections {
            if nss >= nss_num {
                continue;
            }
            for idx in 0..idx_num {
                let d = RateDesc {
                    rs,
                    nss,
                    ofdma: 0,
                    idx,
                };
                v[(idx % 4) as usize] = read_byrate(t, chan.band_type(), 0, d);
                if (idx + 1) % 4 != 0 {
                    continue;
                }
                mac_txpwr_w32(bus, addr, pack(&v), ev)?;
                addr += 4;
            }
        }
    }
    Ok(())
}

// Linux: rtw89_phy_set_txpwr_offset_ax (phy.c:3126)
fn set_offset<B: Bus + ?Sized>(
    bus: &mut B,
    t: &TxpwrTables,
    chan: &RtwChan,
    ev: &mut EventLog<ChanEvent>,
) -> Rtw89Result<()> {
    let mut val = 0u32;
    for idx in 0..RTW89_RATE_OFFSET_NUM_AX {
        let d = RateDesc {
            rs: RTW89_RS_OFFSET,
            nss: RTW89_NSS_1,
            ofdma: 0,
            idx,
        };
        let v = read_byrate(t, chan.band_type(), 0, d);
        val |= (v as u32 & 0xf) << (idx * 4);
    }
    mac_txpwr_mask32(bus, R_AX_PWR_RATE_OFST_CTRL, 0xfffff, val, ev)
}

// Linux: rtw89_get_tx_shape_idx (core.h:8935); this firmware has the v1 table
pub fn tx_shape_idx(t: &TxpwrTables, regd: &Regd, band: u32, rs: u32) -> i8 {
    t.tx_shape_lmt
        .get(&[
            band as usize,
            rs as usize,
            regd_get(regd, band) as usize,
            RTW89_REG_6GHZ_POWER_DFLT as usize,
        ])
        .unwrap_or(0)
}

// Linux: rtw8852bx_set_tx_shape (rtw8852b_common.c:1320) with rtw8852bx_bb_set_tx_shape_dfir
// (rtw8852b_common.c:1269): the CCK DFIR on 2.4 GHz (sharp_14 on channel 14, flat for shape 0),
// the OFDM shape into R_DCFO_OPT
fn set_tx_shape<B: Bus + ?Sized>(
    bus: &mut B,
    t: &TxpwrTables,
    regd: &Regd,
    chan: &RtwChan,
) -> Rtw89Result<()> {
    let band = chan.band_type();
    let cck = tx_shape_idx(t, regd, band, RTW89_RS_CCK);
    let ofdm = tx_shape_idx(t, regd, band, RTW89_RS_OFDM);
    if band == RTW89_BAND_2G {
        let param = if chan.channel == 14 {
            DFIR_SHARP14
        } else if cck == 0 {
            DFIR_FLAT
        } else {
            DFIR_SHARP
        };
        for (i, &v) in param.iter().enumerate() {
            phy_mask32(bus, R_TXFIR0 + ((i as u32) << 2), MASKDWORD, v)?;
        }
    }
    phy_mask32(
        bus,
        R_DCFO_OPT,
        B_TXSHAPE_TRIANGULAR_CFG,
        i32::from(ofdm) as u32,
    )
}

/// struct rtw89_txpwr_limit_ax (phy.h:506) as rtw89_phy_fill_txpwr_limit_20m_ax (phy.c:2660)
/// fills it: cck_20m, cck_40m, ofdm, mcs_20m[0], each [bf 0, bf 1]; the rest 0
pub fn fill_limit_20m(t: &TxpwrTables, regd: &Regd, band: u32, ntx: u32, ch: u32) -> [i8; 40] {
    let mut lmt = [0i8; RTW89_TXPWR_LMT_PAGE_SIZE_AX as usize];
    let parts = [
        (0, RTW89_CHANNEL_WIDTH_20, RTW89_RS_CCK),
        (2, RTW89_CHANNEL_WIDTH_40, RTW89_RS_CCK),
        (4, RTW89_CHANNEL_WIDTH_20, RTW89_RS_OFDM),
        (6, RTW89_CHANNEL_WIDTH_20, RTW89_RS_MCS),
    ];
    for (at, bw, rs) in parts {
        for bf in 0..RTW89_BF_NUM {
            let v = read_txpwr_limit(t, regd, band, bw, ntx, rs, bf, ch);
            lmt[at + bf as usize] = v as i8;
        }
    }
    lmt
}

/// struct rtw89_txpwr_limit_ru_ax (phy.h:539) as rtw89_phy_fill_txpwr_limit_ru_20m_ax
/// (phy.c:2928) fills it: ru26[0], ru52[0], ru106[0]
pub fn fill_limit_ru_20m(t: &TxpwrTables, regd: &Regd, band: u32, ntx: u32, ch: u32) -> [i8; 24] {
    let mut lmt = [0i8; RTW89_TXPWR_LMT_RU_PAGE_SIZE_AX as usize];
    let sec = RTW89_RU_SEC_NUM_AX as usize;
    for (k, ru) in [RTW89_RU26, RTW89_RU52, RTW89_RU106]
        .into_iter()
        .enumerate()
    {
        lmt[k * sec] = read_txpwr_limit_ru(t, regd, band, ru, ntx, ch) as i8;
    }
    lmt
}

fn dword(b: &[i8]) -> u32 {
    b.iter()
        .take(4)
        .enumerate()
        .fold(0u32, |a, (i, &x)| a | (u32::from(x as u8) << (8 * i)))
}

// Linux: rtw89_phy_set_txpwr_limit_ax (phy.c:3154) and _limit_ru_ax (phy.c:3189), 20 MHz
fn set_limits<B: Bus + ?Sized>(
    bus: &mut B,
    t: &TxpwrTables,
    regd: &Regd,
    chan: &RtwChan,
    ev: &mut EventLog<ChanEvent>,
) -> Rtw89Result<()> {
    let band = chan.band_type();
    let mut addr = R_AX_PWR_LMT;
    for ntx in 0..RTW89_NTX_NUM {
        let lmt = fill_limit_20m(t, regd, band, ntx, chan.channel);
        for j in lmt.chunks(4) {
            mac_txpwr_w32(bus, addr, dword(j), ev)?;
            addr += 4;
        }
    }
    let mut addr = R_AX_PWR_RU_LMT;
    for ntx in 0..RTW89_NTX_NUM {
        let lmt = fill_limit_ru_20m(t, regd, band, ntx, chan.channel);
        for j in lmt.chunks(4) {
            mac_txpwr_w32(bus, addr, dword(j), ev)?;
            addr += 4;
        }
    }
    Ok(())
}

// Linux: rtw8852bx_bb_cal_txpwr_ref (rtw8852b_common.c:1205): `dec` in 1/8 dB
// (the C's int arithmetic wraps here instead of being undefined)
pub fn cal_txpwr_ref(r: i32, dec: i32) -> u32 {
    let r2 = r.wrapping_shl(1);
    let pwr = (r2.wrapping_add(0x27 << 3).wrapping_sub(dec) as i16) as i32; // s16 pwr_s10_3
    let bb_cw = pwr & 0x7;
    let rf_cw = ((pwr & 0x1f8) >> 3).clamp(15, 63);
    let pwr_cw = (rf_cw << 3) | bb_cw;
    let tssi_ofst_cw = (0x12c_i32.wrapping_add(r2).wrapping_sub(16 << 3)).wrapping_sub(dec) as u32;
    prep(B_DPD_TSSI_CW, tssi_ofst_cw)
        | prep(B_DPD_PWR_CW, pwr_cw as u32)
        | prep(B_DPD_REF, r as u32)
}

// Linux: rtw8852bx_set_txpwr_ref (rtw8852b_common.c:1235) with reference 0 for OFDM and CCK;
// `pwr_ofst` is path A minus path B in 1/8 dB
pub fn set_txpwr_ref<B: Bus + ?Sized>(
    bus: &mut B,
    pwr_ofst: i32,
    ev: &mut EventLog<ChanEvent>,
) -> Rtw89Result<()> {
    let mask = B_DPD_TSSI_CW | B_DPD_PWR_CW | B_DPD_REF;
    mac_txpwr_mask32(bus, R_AX_PWR_RATE_CTRL, B_AX_PWR_REF, 0x0, ev)?;
    let dec = [
        if pwr_ofst > 0 {
            0
        } else {
            pwr_ofst.saturating_abs()
        },
        if pwr_ofst > 0 { pwr_ofst } else { 0 },
    ];
    for ofst in [0x4, 0x8] {
        // OFDM, then CCK
        for (i, base) in [0x5800u32, 0x7800].into_iter().enumerate() {
            phy_mask32(bus, base + ofst, mask, cal_txpwr_ref(0, dec[i]))?;
        }
    }
    Ok(())
}

// Linux: __rtw8852bx_set_txpwr (rtw8852b_common.c:1367), the chip's set_txpwr, with
// rtw8852bx_set_txpwr_diff: antenna gain offset 0 (no DA tables), the SAR difference path A -
// path B is 0 (both RTW89_SAR_TXPWR_MAC_MAX without a SAR source)
pub fn set_txpwr_chip<B: Bus + ?Sized>(
    bus: &mut B,
    t: &TxpwrTables,
    regd: &Regd,
    chan: &RtwChan,
    ev: &mut EventLog<ChanEvent>,
) -> Rtw89Result<()> {
    set_byrate(bus, t, chan, ev)?;
    set_offset(bus, t, chan, ev)?;
    set_tx_shape(bus, t, regd, chan)?;
    set_limits(bus, t, regd, chan, ev)?;
    let pwr_ofst = mac_to_bb(SAR_TXPWR_MAC_MAX) - mac_to_bb(SAR_TXPWR_MAC_MAX);
    set_txpwr_ref(bus, pwr_ofst, ev)
}

// Linux: __rtw8852bx_set_txpwr_ul_tb_offset (rtw8852b_common.c:1386), MAC 0 (plain register
// writes, no TX power CR check)
fn set_ul_tb_offset<B: Bus + ?Sized>(bus: &mut B, pw_ofst: i32) -> Rtw89Result<()> {
    if !(-16..=15).contains(&pw_ofst) {
        return Ok(());
    }
    bus.set32(R_AX_PWR_UL_TB_CTRL, B_AX_PWR_UL_TB_CTRL_EN)?;
    bus.mask32(R_AX_PWR_UL_TB_1T, B_AX_PWR_UL_TB_1T_MASK, pw_ofst as u32)?;
    bus.mask32(
        R_AX_PWR_UL_TB_2T,
        B_AX_PWR_UL_TB_2T_MASK,
        (pw_ofst - 3).max(-16) as u32,
    )?;
    Ok(())
}

// Linux: __rtw8852bx_init_txpwr_unit (rtw8852b_common.c:1407), PHY 0
pub fn init_txpwr_unit_chip<B: Bus + ?Sized>(
    bus: &mut B,
    ev: &mut EventLog<ChanEvent>,
) -> Rtw89Result<()> {
    mac_txpwr_w32(bus, R_AX_PWR_UL_CTRL2, 0x07763333, ev)?;
    mac_txpwr_w32(bus, R_AX_PWR_COEXT_CTRL, 0x01ebf000, ev)?;
    mac_txpwr_w32(bus, R_AX_PWR_UL_CTRL0, 0x0002f8ff, ev)?;
    set_ul_tb_offset(bus, 0)
}
