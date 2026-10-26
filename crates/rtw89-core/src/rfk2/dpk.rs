//! The steps of the digital predistortion calibration (DPK) of Linux rtw8852b_rfk.c 1649-2406
//! (JS js/rfk2-dpk.mjs): KIP one-shot commands, RF and AFE settings, sync and gain loss reports
//! and the TX AGC search. The flow that strings them together is in `dpkflow`.

use alloc::vec::Vec;

use crate::bus::{Bus, field};
use crate::error::{Rtw89Error, Rtw89Result};
use crate::phy::io::{
    MASKDWORD, RFREG_MASK, phy_clr32, phy_mask32, phy_r32, phy_r32_mask, rf_r, rf_w,
};
use crate::rfk1::util::{RfkChan, rfk_parser, set_rx_dck};
use crate::tables::regs::*;
use crate::tables::rfk2::{DPK_AFE_DEFS, DPK_AFE_RESTORE_DEFS, DPK_KIP_DEFS};

use super::{Rfk2State, poll_soft, s8, sign_extend32};

// enum rtw8852b_dpk_id (rtw8852b_rfk.c:35), the ones the 8852B flow uses
pub const LBK_RXIQK: u32 = 0x06;
pub const SYNC: u32 = 0x10;
pub const MDPK_IDL: u32 = 0x11;
pub const GAIN_LOSS: u32 = 0x13;
pub const DPK_RXAGC: u32 = 0x15;
pub const KIP_PRESET: u32 = 0x16;
pub const DPK_TXAGC: u32 = 0x19;
const DPK_TXAGC_LOWER: i32 = 0x2e;
const DPK_TXAGC_UPPER: i32 = 0x3f;
/// _dpk_agc's answer when the sync failed
pub const DPK_TXAGC_INVAL: u32 = 0xff;
const RFREG_MASKRXBB: u32 = 0x003e0;
const RFREG_MASKMODE: u32 = 0xf0000;

// Linux: _dpk_bkup_kip (rtw8852b_rfk.c:1649)
pub fn dpk_bkup_kip<B: Bus + ?Sized>(bus: &mut B, reg: &[u32], path: u32) -> Rtw89Result<Vec<u32>> {
    reg.iter()
        .map(|&a| phy_r32(bus, a.wrapping_add(path << 8)))
        .collect()
}

// Linux: _dpk_reload_kip (rtw8852b_rfk.c:1662)
pub fn dpk_reload_kip<B: Bus + ?Sized>(
    bus: &mut B,
    reg: &[u32],
    bkup: &[u32],
    path: u32,
) -> Rtw89Result<()> {
    for (&a, &v) in reg.iter().zip(bkup) {
        phy_mask32(bus, a.wrapping_add(path << 8), MASKDWORD, v)?;
    }
    Ok(())
}

// Linux: _dpk_order_convert (rtw8852b_rfk.c:1675)
fn dpk_order_convert<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<u32> {
    let order = phy_r32_mask(bus, R_LDL_NORM, B_LDL_NORM_OP)?;
    Ok(0x3u32.wrapping_shr(order))
}

// Linux: _dpk_onoff (rtw8852b_rfk.c:1688)
pub fn dpk_onoff<B: Bus + ?Sized>(
    bus: &mut B,
    st: &Rfk2State,
    path: u32,
    off: bool,
) -> Rtw89Result<()> {
    let kidx = st.dpk.slot(path)?;
    let path_ok = st.dpk.bp[path as usize][kidx].path_ok; // slot() checked the path
    let val = u32::from(st.dpk.is_dpk_enable && !off && path_ok);
    let order = dpk_order_convert(bus)?;
    let a = R_DPD_CH0A + (path << 8) + ((kidx as u32) << 2);
    phy_mask32(bus, a, MASKBYTE3, (order << 1) | val)
}

// Linux: _dpk_one_shot (rtw8852b_rfk.c:1702); both waits only log a timeout
pub fn dpk_one_shot<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut Rfk2State,
    _phy: u32,
    path: u32,
    id: u32,
) -> Rtw89Result<()> {
    crate::rfk1::path_idx(path)?;
    let dpk_cmd = id.wrapping_shl(8) | (0x19 + (path << 4));
    phy_mask32(bus, R_NCTL_CFG, MASKDWORD, dpk_cmd)?;
    poll_soft(bus, st, path, 0xbff8, MASKBYTE0, 0x55, 20000)?;
    bus.delay_us(1)?;
    phy_mask32(bus, R_KIP_RPT1, MASKDWORD, 0x0003_0000)?;
    poll_soft(bus, st, path, 0x80fc, MASKLWORD, 0x8000, 2000)?;
    phy_mask32(bus, R_NCTL_N1, MASKBYTE0, 0x0)
}

// Linux: _dpk_rx_dck (rtw8852b_rfk.c:1744)
pub fn dpk_rx_dck<B: Bus + ?Sized>(bus: &mut B, phy: u32, path: u32) -> Rtw89Result<()> {
    rf_w(bus, path, RR_RXBB2, RR_EN_TIA_IDA, 0x3)?;
    set_rx_dck(bus, phy, path)
}

// Linux: _dpk_information (rtw8852b_rfk.c:1751)
pub fn dpk_information(st: &mut Rfk2State, path: u32, c: &RfkChan) -> Rtw89Result<()> {
    let kidx = st.dpk.slot(path)?;
    let bp = st.dpk.at(path, kidx)?;
    bp.band = c.band_type;
    bp.ch = c.channel;
    bp.bw = c.band_width;
    Ok(())
}

// Linux: _dpk_bb_afe_setting (rtw8852b_rfk.c:1775)
pub fn dpk_bb_afe_setting<B: Bus + ?Sized>(bus: &mut B, c: &RfkChan) -> Rtw89Result<()> {
    rfk_parser(bus, DPK_AFE_DEFS)?;
    if c.band_width == RTW89_CHANNEL_WIDTH_80 {
        phy_mask32(bus, R_P0_CFCH_BW1, B_P0_CFCH_EX, 0x1)?;
        phy_mask32(bus, R_PATH1_BW_SEL_V1, B_PATH1_BW_SEL_EX, 0x1)?;
    }
    Ok(())
}

// Linux: _dpk_bb_afe_restore (rtw8852b_rfk.c:1793)
pub fn dpk_bb_afe_restore<B: Bus + ?Sized>(bus: &mut B, c: &RfkChan) -> Rtw89Result<()> {
    rfk_parser(bus, DPK_AFE_RESTORE_DEFS)?;
    if c.band_width == RTW89_CHANNEL_WIDTH_80 {
        phy_mask32(bus, R_P0_CFCH_BW1, B_P0_CFCH_EX, 0x0)?;
        phy_mask32(bus, R_PATH1_BW_SEL_V1, B_PATH1_BW_SEL_EX, 0x0)?;
    }
    Ok(())
}

// Linux: _dpk_tssi_pause (rtw8852b_rfk.c:1811)
pub fn dpk_tssi_pause<B: Bus + ?Sized>(bus: &mut B, path: u32, is_pause: bool) -> Rtw89Result<()> {
    crate::rfk1::path_idx(path)?;
    phy_mask32(
        bus,
        R_P0_TSSI_TRK + (path << 13),
        B_P0_TSSI_TRK_EN,
        u32::from(is_pause),
    )
}

// Linux: _dpk_kip_restore (rtw8852b_rfk.c:1821); 8852B: hal.cv is CHIP_CBV, above CHIP_CAV
pub fn dpk_kip_restore<B: Bus + ?Sized>(bus: &mut B, path: u32) -> Rtw89Result<()> {
    crate::rfk1::path_idx(path)?;
    rfk_parser(bus, DPK_KIP_DEFS)?;
    phy_mask32(bus, R_DPD_COM + (path << 8), B_DPD_COM_OF, 0x1)
}

// Linux: _dpk_lbk_rxiqk (rtw8852b_rfk.c:1832); the RXIQC read for the log is left out
fn dpk_lbk_rxiqk<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut Rfk2State,
    phy: u32,
    path: u32,
) -> Rtw89Result<()> {
    let cur_rxbb = rf_r(bus, path, RR_MOD, RFREG_MASKRXBB)?;
    phy_mask32(bus, R_MDPK_RX_DCK, B_MDPK_RX_DCK_EN, 0x1)?;
    phy_mask32(bus, R_IQK_RES + (path << 8), B_IQK_RES_RXCFIR, 0x0)?;
    let tmp = rf_r(bus, path, RR_CFGCH, RFREG_MASK)?;
    rf_w(bus, path, RR_RSV4, RFREG_MASK, tmp)?;
    rf_w(bus, path, RR_MOD, RFREG_MASKMODE, 0xd)?;
    rf_w(bus, path, RR_RXK, RR_RXK_PLLEN, 0x1)?;
    let att1 = if cur_rxbb >= 0x11 {
        0x13
    } else if cur_rxbb <= 0xa {
        0x00
    } else {
        0x05
    };
    rf_w(bus, path, RR_TXIQK, RR_TXIQK_ATT1, att1)?;
    rf_w(bus, path, RR_XGLNA2, RR_XGLNA2_SW, 0x0)?;
    rf_w(bus, path, RR_RXKPLL, RR_RXKPLL_POW, 0x0)?;
    rf_w(bus, path, RR_RXKPLL, RFREG_MASK, 0x80014)?;
    bus.delay_us(70)?;
    phy_mask32(bus, R_P0_RFCTM, B_P0_RFCTM_EN, 0x1)?;
    phy_mask32(bus, R_IQK_DIF4, B_IQK_DIF4_RXT, 0x025)?;
    dpk_one_shot(bus, st, phy, path, LBK_RXIQK)?;
    phy_mask32(bus, R_P0_RFCTM, B_P0_RFCTM_EN, 0x0)?;
    rf_w(bus, path, RR_RXK, RR_RXK_PLLEN, 0x0)?;
    phy_mask32(bus, R_MDPK_RX_DCK, B_MDPK_RX_DCK_EN, 0x0)?;
    phy_mask32(bus, R_KPATH_CFG, B_KPATH_CFG_ED, 0x0)?;
    phy_mask32(bus, R_LOAD_COEF + (path << 8), B_LOAD_COEF_DI, 0x1)?;
    rf_w(bus, path, RR_MOD, RFREG_MASKMODE, 0x5)
}

// Linux: _dpk_get_thermal (rtw8852b_rfk.c:1876)
pub fn dpk_get_thermal<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut Rfk2State,
    kidx: usize,
    path: u32,
) -> Rtw89Result<()> {
    rf_w(bus, path, RR_TM, RR_TM_TRI, 0x1)?;
    rf_w(bus, path, RR_TM, RR_TM_TRI, 0x0)?;
    rf_w(bus, path, RR_TM, RR_TM_TRI, 0x1)?;
    bus.delay_us(200)?;
    let v = rf_r(bus, path, RR_TM, RR_TM_VAL)?;
    st.dpk.at(path, kidx)?.ther_dpk = v;
    Ok(())
}

// Linux: _dpk_rf_setting (rtw8852b_rfk.c:1892); the RF reads for the log are left out
pub fn dpk_rf_setting<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut Rfk2State,
    _gain: u32,
    path: u32,
    kidx: usize,
) -> Rtw89Result<()> {
    let bp = *st.dpk.at(path, kidx)?;
    rf_w(bus, path, RR_MOD, RFREG_MASK, 0x50220)?;
    if bp.band == RTW89_BAND_2G {
        rf_w(bus, path, RR_RXBB, RR_RXBB_FATT, 0xf2)?;
        rf_w(bus, path, RR_LUTDBG, RR_LUTDBG_TIA, 0x1)?;
        rf_w(bus, path, RR_TIA, RR_TIA_N6, 0x1)?;
    } else {
        rf_w(bus, path, RR_RXA2, RR_RAA2_SWATT, 0x5)?;
        rf_w(bus, path, RR_LUTDBG, RR_LUTDBG_TIA, 0x1)?;
        rf_w(bus, path, RR_TIA, RR_TIA_N6, 0x1)?;
        rf_w(bus, path, RR_RXA_LNA, RFREG_MASK, 0x920FC)?;
        rf_w(bus, path, RR_XALNA2, RFREG_MASK, 0x002C0)?;
        rf_w(bus, path, RR_IQGEN, RFREG_MASK, 0x38800)?;
    }
    rf_w(bus, path, RR_RCKD, RR_RCKD_BW, 0x1)?;
    rf_w(bus, path, RR_BTC, RR_BTC_TXBB, bp.bw.wrapping_add(1))?;
    rf_w(bus, path, RR_BTC, RR_BTC_RXBB, 0x0)
}

// Linux: _dpk_bypass_rxcfir (rtw8852b_rfk.c:1923); the read for the log is left out
fn dpk_bypass_rxcfir<B: Bus + ?Sized>(bus: &mut B, path: u32, is_bypass: bool) -> Rtw89Result<()> {
    let a = R_RXIQC + (path << 8);
    if is_bypass {
        phy_mask32(bus, a, B_RXIQC_BYPASS2, 0x1)?;
        phy_mask32(bus, a, B_RXIQC_BYPASS, 0x1)
    } else {
        phy_clr32(bus, a, B_RXIQC_BYPASS2)?;
        phy_clr32(bus, a, B_RXIQC_BYPASS)
    }
}

// Linux: _dpk_tpg_sel (rtw8852b_rfk.c:1946)
fn dpk_tpg_sel<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut Rfk2State,
    path: u32,
    kidx: usize,
) -> Rtw89Result<()> {
    let bw = st.dpk.at(path, kidx)?.bw;
    if bw == RTW89_CHANNEL_WIDTH_80 {
        phy_clr32(bus, R_TPG_MOD, B_TPG_MOD_F)
    } else if bw == RTW89_CHANNEL_WIDTH_40 {
        phy_mask32(bus, R_TPG_MOD, B_TPG_MOD_F, 0x2)
    } else {
        phy_mask32(bus, R_TPG_MOD, B_TPG_MOD_F, 0x1)
    }
}

// Linux: _dpk_table_select (rtw8852b_rfk.c:1962)
pub fn dpk_table_select<B: Bus + ?Sized>(
    bus: &mut B,
    path: u32,
    kidx: usize,
    gain: u32,
) -> Rtw89Result<()> {
    crate::rfk1::path_idx(path)?;
    let val = 0x80u32
        .wrapping_add((kidx as u32).wrapping_mul(0x20))
        .wrapping_add(gain.wrapping_mul(0x10));
    phy_mask32(bus, R_DPD_CH0 + (path << 8), MASKBYTE3, val)
}

// R_RPT_COM twice in a row, as Linux reads it
fn read_rpt2<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<(u32, u32)> {
    Ok((phy_r32(bus, R_RPT_COM)?, phy_r32(bus, R_RPT_COM)?))
}

// Linux: _dpk_sync_check (rtw8852b_rfk.c:1974): true when the sync failed
fn dpk_sync_check<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut Rfk2State,
    path: u32,
    kidx: usize,
) -> Rtw89Result<bool> {
    st.dpk.at(path, kidx)?; // path and slot in range
    let p = path as usize;
    phy_clr32(bus, R_KIP_RPT1, B_KIP_RPT1_SEL)?;
    let (a, b) = read_rpt2(bus)?;
    let corr_idx = field(a, B_PRT_COM_CORI);
    let corr_val = field(b, B_PRT_COM_CORV);
    st.dpk.corr_idx[p][kidx] = corr_idx;
    st.dpk.corr_val[p][kidx] = corr_val;
    phy_mask32(bus, R_KIP_RPT1, B_KIP_RPT1_SEL, 0x9)?;
    let (a, b) = read_rpt2(bus)?;
    let dc_i = sign_extend32(field(a, B_PRT_COM_DCI), 11).unsigned_abs();
    let dc_q = sign_extend32(field(b, B_PRT_COM_DCQ), 11).unsigned_abs();
    st.dpk.dc_i[p][kidx] = dc_i;
    st.dpk.dc_q[p][kidx] = dc_q;
    // DPK_SYNC_TH_DC_I / _DC_Q / _CORR
    Ok(dc_i > 200 || dc_q > 200 || corr_val < 170)
}

// Linux: _dpk_sync (rtw8852b_rfk.c:2016)
fn dpk_sync<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut Rfk2State,
    phy: u32,
    path: u32,
    kidx: usize,
) -> Rtw89Result<bool> {
    dpk_one_shot(bus, st, phy, path, SYNC)?;
    dpk_sync_check(bus, st, path, kidx)
}

// Linux: _dpk_dgain_read (rtw8852b_rfk.c:2024)
fn dpk_dgain_read<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<u32> {
    phy_mask32(bus, R_KIP_RPT1, B_KIP_RPT1_SEL, 0x0)?;
    phy_r32_mask(bus, R_RPT_COM, B_PRT_COM_DCI)
}

// Linux: _dpk_dgain_mapping (rtw8852b_rfk.c:2037): the s8 RXBB step for a digital gain
pub fn dpk_dgain_mapping(dgain: u32) -> i32 {
    const BND: [u32; 15] = [
        0xbf1, 0xaa5, 0x97d, 0x875, 0x789, 0x6b7, 0x5fc, 0x556, 0x4c1, 0x43d, 0x3c7, 0x35e, 0x2ac,
        0x262, 0x220,
    ];
    const OFFSET: [i32; 15] = [6, 6, 5, 4, 3, 2, 1, 0, -1, -2, -3, -4, -5, -6, -7];
    BND.iter()
        .zip(OFFSET)
        .find(|&(&b, _)| dgain >= b)
        .map_or(-8, |(_, o)| o)
}

// Linux: _dpk_gainloss_read (rtw8852b_rfk.c:2085)
fn dpk_gainloss_read<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<u32> {
    phy_mask32(bus, R_KIP_RPT1, B_KIP_RPT1_SEL, 0x6)?;
    phy_mask32(bus, R_DPK_CFG2, B_DPK_CFG2_ST, 0x1)?;
    phy_r32_mask(bus, R_RPT_COM, B_PRT_COM_GL)
}

// Linux: _dpk_gainloss (rtw8852b_rfk.c:2093)
fn dpk_gainloss<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut Rfk2State,
    phy: u32,
    path: u32,
    kidx: usize,
) -> Rtw89Result<()> {
    dpk_table_select(bus, path, kidx, 1)?;
    dpk_one_shot(bus, st, phy, path, GAIN_LOSS)
}

// Linux: _dpk_kip_preset (rtw8852b_rfk.c:2100)
pub fn dpk_kip_preset<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut Rfk2State,
    phy: u32,
    path: u32,
    kidx: usize,
) -> Rtw89Result<()> {
    dpk_tpg_sel(bus, st, path, kidx)?;
    dpk_one_shot(bus, st, phy, path, KIP_PRESET)
}

// Linux: _dpk_kip_pwr_clk_on (rtw8852b_rfk.c:2107)
pub fn dpk_kip_pwr_clk_on<B: Bus + ?Sized>(bus: &mut B, path: u32) -> Rtw89Result<()> {
    crate::rfk1::path_idx(path)?;
    phy_mask32(bus, R_NCTL_RPT, MASKDWORD, 0x0000_0080)?;
    phy_mask32(bus, R_KIP_SYSCFG, MASKDWORD, 0x807f_030a)?;
    phy_mask32(bus, R_CFIR_SYS + (path << 8), MASKDWORD, 0xce00_0a08)
}

// Linux: _dpk_kip_set_txagc (rtw8852b_rfk.c:2117)
pub fn dpk_kip_set_txagc<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut Rfk2State,
    phy: u32,
    path: u32,
    txagc: u32,
) -> Rtw89Result<()> {
    rf_w(bus, path, RR_TXAGC, RFREG_MASK, txagc)?;
    phy_mask32(bus, R_P0_RFCTM, B_P0_RFCTM_EN, 0x1)?;
    dpk_one_shot(bus, st, phy, path, DPK_TXAGC)?;
    phy_mask32(bus, R_P0_RFCTM, B_P0_RFCTM_EN, 0x0)
}

// Linux: _dpk_kip_set_rxagc (rtw8852b_rfk.c:2128); the reads for the log are left out
pub fn dpk_kip_set_rxagc<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut Rfk2State,
    phy: u32,
    path: u32,
) -> Rtw89Result<()> {
    let tmp = rf_r(bus, path, RR_MOD, RFREG_MASK)?;
    phy_mask32(bus, R_KIP_MOD, B_KIP_MOD, tmp)?;
    phy_mask32(bus, R_P0_RFCTM, B_P0_RFCTM_EN, 0x1)?;
    dpk_one_shot(bus, st, phy, path, DPK_RXAGC)?;
    phy_mask32(bus, R_P0_RFCTM, B_P0_RFCTM_EN, 0x0)?;
    phy_mask32(bus, R_KIP_RPT1, B_KIP_RPT1_SEL_V1, 0x8)
}

// Linux: _dpk_set_offset (rtw8852b_rfk.c:2146); `gain_offset` is s8, the TX AGC u8
fn dpk_set_offset<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut Rfk2State,
    phy: u32,
    path: u32,
    gain_offset: i32,
) -> Rtw89Result<u32> {
    let cur = (rf_r(bus, path, RR_TXAGC, RFREG_MASK)? & 0xff) as i32;
    let want = cur - gain_offset;
    let txagc = want.clamp(DPK_TXAGC_LOWER, DPK_TXAGC_UPPER) as u32;
    dpk_kip_set_txagc(bus, st, phy, path, txagc)?;
    Ok(txagc)
}

// Linux: _dpk_pas_read (rtw8852b_rfk.c:2167), is_check true (its only use)
fn dpk_pas_read<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<bool> {
    let abs12 = |v: u32, mask: u32| sign_extend32(field(v, mask), 11).unsigned_abs();
    phy_mask32(bus, R_KIP_RPT1, MASKBYTE2, 0x06)?;
    phy_mask32(bus, R_DPK_CFG2, B_DPK_CFG2_ST, 0x0)?;
    phy_mask32(bus, R_DPK_CFG3, MASKBYTE2, 0x08)?;
    phy_mask32(bus, R_DPK_CFG3, MASKBYTE3, 0x00)?;
    let (hi, lo) = read_rpt2(bus)?;
    let (v1i, v1q) = (abs12(hi, MASKHWORD), abs12(lo, MASKLWORD));
    phy_mask32(bus, R_DPK_CFG3, MASKBYTE3, 0x1f)?;
    let (hi, lo) = read_rpt2(bus)?;
    let (v2i, v2q) = (abs12(hi, MASKHWORD), abs12(lo, MASKLWORD));
    // u32 arithmetic, integer division as in C (at most 2048^2 * 2 * 8)
    Ok(v1i * v1i + v1q * v1q >= (v2i * v2i + v2q * v2q) * 8 / 5)
}

/// enum dpk_agc_step (rtw8852b_rfk.c:57)
#[derive(Clone, Copy, PartialEq, Eq)]
enum AgcStep {
    SyncDgain,
    GainAdj,
    GainLossIdx,
    GlGtCriterion,
    GlLtCriterion,
    SetTxGain,
}

// Linux: _dpk_agc (rtw8852b_rfk.c:2208): the TX AGC for DPK, DPK_TXAGC_INVAL when the sync
// failed. At most 6 AGC steps (and Linux's `limit` of 200 loops).
#[allow(clippy::too_many_arguments)]
pub fn dpk_agc<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut Rfk2State,
    phy: u32,
    path: u32,
    kidx: usize,
    init_txagc: u32,
    loss_only: bool,
    c: &RfkChan,
) -> Rtw89Result<u32> {
    let mut step = AgcStep::SyncDgain;
    let mut tmp_txagc = init_txagc;
    let mut tmp_gl_idx: u32 = 0;
    let mut goout = false;
    let mut agc_cnt = 0u32;
    let mut limited_rxbb = false;
    let mut dgain = 0u32;
    let mut limit: i32 = 200;
    loop {
        match step {
            AgcStep::SyncDgain => {
                if dpk_sync(bus, st, phy, path, kidx)? {
                    tmp_txagc = DPK_TXAGC_INVAL;
                    goout = true;
                } else {
                    dgain = dpk_dgain_read(bus)?;
                    step = if loss_only || limited_rxbb {
                        AgcStep::GainLossIdx
                    } else {
                        AgcStep::GainAdj
                    };
                }
            }
            AgcStep::GainAdj => {
                let rxbb = rf_r(bus, path, RR_MOD, RFREG_MASKRXBB)? as i32;
                let offset = dpk_dgain_mapping(dgain);
                let tmp_rxbb = if rxbb + offset > 0x1f {
                    limited_rxbb = true;
                    0x1f
                } else if rxbb + offset < 0 {
                    limited_rxbb = true;
                    0
                } else {
                    rxbb + offset
                };
                rf_w(bus, path, RR_MOD, RFREG_MASKRXBB, tmp_rxbb as u32)?;
                if offset != 0 || agc_cnt == 0 {
                    if c.band_width < RTW89_CHANNEL_WIDTH_80 {
                        dpk_bypass_rxcfir(bus, path, true)?;
                    } else {
                        dpk_lbk_rxiqk(bus, st, phy, path)?;
                    }
                }
                step = if !(342..=1922).contains(&dgain) {
                    AgcStep::SyncDgain
                } else {
                    AgcStep::GainLossIdx
                };
                agc_cnt += 1;
            }
            AgcStep::GainLossIdx => {
                dpk_gainloss(bus, st, phy, path, kidx)?;
                tmp_gl_idx = dpk_gainloss_read(bus)?;
                step = if (tmp_gl_idx == 0 && dpk_pas_read(bus)?) || tmp_gl_idx >= 7 {
                    AgcStep::GlGtCriterion
                } else if tmp_gl_idx == 0 {
                    AgcStep::GlLtCriterion
                } else {
                    AgcStep::SetTxGain
                };
            }
            AgcStep::GlGtCriterion => {
                if tmp_txagc == 0x2e {
                    goout = true;
                } else {
                    tmp_txagc = dpk_set_offset(bus, st, phy, path, 0x3)?;
                }
                step = AgcStep::GainLossIdx;
                agc_cnt += 1;
            }
            AgcStep::GlLtCriterion => {
                if tmp_txagc == 0x3f {
                    goout = true;
                } else {
                    tmp_txagc = dpk_set_offset(bus, st, phy, path, s8(0xfe))?;
                }
                step = AgcStep::GainLossIdx;
                agc_cnt += 1;
            }
            AgcStep::SetTxGain => {
                // the u8 tmp_gl_idx passed as s8 gain_offset
                tmp_txagc = dpk_set_offset(bus, st, phy, path, s8(tmp_gl_idx as i32))?;
                goout = true;
                agc_cnt += 1;
            }
        }
        // do { } while (!goout && agc_cnt < 6 && limit-- > 0)
        let again = !goout && agc_cnt < 6 && {
            let l = limit;
            limit -= 1;
            l > 0
        };
        if !again {
            return Ok(tmp_txagc);
        }
    }
}

// Linux: _dpk_set_mdpd_para (rtw8852b_rfk.c:2327), orders 0 and 2 used
fn dpk_set_mdpd_para<B: Bus + ?Sized>(bus: &mut B, order: u32) -> Rtw89Result<()> {
    phy_mask32(bus, R_LDL_NORM, B_LDL_NORM_OP, order)?;
    if order == 0 {
        phy_mask32(bus, R_LDL_NORM, B_LDL_NORM_PN, 0x3)?;
        phy_mask32(bus, R_MDPK_SYNC, B_MDPK_SYNC_MAN, 0x1)
    } else {
        phy_clr32(bus, R_LDL_NORM, B_LDL_NORM_PN)?;
        phy_clr32(bus, R_MDPK_SYNC, B_MDPK_SYNC_MAN)
    }
}

// Linux: _dpk_idl_mpa (rtw8852b_rfk.c:2355)
pub fn dpk_idl_mpa<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut Rfk2State,
    phy: u32,
    path: u32,
    kidx: usize,
) -> Rtw89Result<()> {
    let bp = *st.dpk.at(path, kidx)?;
    if bp.bw < RTW89_CHANNEL_WIDTH_80 && bp.band == RTW89_BAND_5G {
        dpk_set_mdpd_para(bus, 0x2)?;
    } else {
        dpk_set_mdpd_para(bus, 0x0)?;
    }
    dpk_one_shot(bus, st, phy, path, MDPK_IDL)
}

// Linux: _dpk_fill_result (rtw8852b_rfk.c:2369)
pub fn dpk_fill_result<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut Rfk2State,
    phy: usize,
    path: u32,
    kidx: usize,
    gain: u32,
    txagc: u32,
) -> Rtw89Result<()> {
    let pwsf = 0x78;
    st.dpk.at(path, kidx)?; // path and slot in range
    let gs = *st.dpk.dpk_gs.get(phy).ok_or(Rtw89Error::Invalid {
        what: "rfk2: PHY index past the chip's two",
        value: phy as u32,
    })?;
    let k = kidx as u32;
    let ch0a = R_DPD_CH0A + (path << 8) + (k << 2);
    phy_mask32(bus, R_COEF_SEL + (path << 8), B_COEF_SEL_MDPD, k)?;
    let bp = &mut st.dpk.bp[path as usize][kidx];
    bp.txagc_dpk = txagc;
    let m = 0x3fu32.wrapping_shl((gain << 3).wrapping_add(k << 4));
    phy_mask32(bus, R_TXAGC_RFK + (path << 8), m, txagc)?;
    bp.pwsf = pwsf;
    let m = 0x1ffu32.wrapping_shl(gain << 4);
    phy_mask32(bus, R_DPD_BND + (path << 8) + (k << 2), m, pwsf)?;
    phy_mask32(bus, R_LOAD_COEF + (path << 8), B_LOAD_COEF_MDPD, 0x1)?;
    phy_mask32(bus, R_LOAD_COEF + (path << 8), B_LOAD_COEF_MDPD, 0x0)?;
    bp.gs = gs;
    let v = if gs == 0x7f { 0x007f_7f7f } else { 0x005b_5b5b };
    phy_mask32(bus, ch0a, MASKDWORD, v)?;
    let order = dpk_order_convert(bus)?;
    phy_mask32(bus, ch0a, B_DPD_ORDER_V1, order)?;
    phy_mask32(bus, R_DPD_V1 + (path << 8), MASKDWORD, 0x0)?;
    phy_mask32(bus, R_MDPK_SYNC, B_MDPK_SYNC_SEL, 0x0)
}
