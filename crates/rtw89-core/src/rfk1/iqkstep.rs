//! The steps of one IQK path in rtw8852b_rfk.c (JS js/rfk1-iqkstep.mjs): the one-shot command
//! to the calibration engine (NCTL), LO leakage (LOK), TX and RX IQ (TXK, RXK by gain group)
//! and their clock and RF settings.

use crate::bus::{Bus, field};
use crate::error::Rtw89Result;
use crate::phy::io::{MASKDWORD, RFREG_MASK, phy_mask32, phy_r32_mask, rf_r, rf_w};
use crate::tables::regs::*;
use crate::tables::rfk1 as t;

use super::util::{debug_bb_read, debug_rf_read, poll_read};
use super::{RfkEvent, RfkState, path_idx};

/// enum rtw8852b_iqk_type (rtw8852b_rfk.c:66)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IqkType {
    TxAgc = 0x0,
    FlokCoarse = 0x1,
    FlokFine = 0x2,
    Txk = 0x3,
    RxAgc = 0x4,
    Rxk = 0x5,
    NbTxk = 0x6,
    NbRxk = 0x7,
    FlokVbuffer = 0x8,
}

/// rtw8852b_rfk.c:20 RTW8852B_RXK_GROUP_NR: the gain groups of TXK and RXK
const RXK_GROUP_NR: u32 = 4;

fn is_2g(st: &RfkState, p: usize) -> bool {
    st.iqk.iqk_band[p] == RTW89_BAND_2G
}
fn is_5g(st: &RfkState, p: usize) -> bool {
    st.iqk.iqk_band[p] == RTW89_BAND_5G
}

// Linux: _iqk_check_cal (rtw8852b_rfk.c:253): the engine writes 0x55 when done; a timeout
// counts as a failed calibration and Linux goes on. Returns `fail`.
pub fn iqk_check_cal<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut RfkState,
    path: u32,
) -> Rtw89Result<bool> {
    let mut fail = true;
    let poll = poll_read(
        bus,
        |b| phy_r32_mask(b, 0xbff8, MASKBYTE0),
        |v| v == 0x55,
        1,
        8200,
    )?;
    bus.delay_us(200)?;
    if !poll.timed_out {
        fail = phy_r32_mask(bus, R_NCTL_RPT, B_NCTL_RPT_FLG)? != 0;
    } else {
        st.push(RfkEvent::NctlTimeout {
            path,
            value: poll.val,
        });
    }
    phy_mask32(bus, R_NCTL_N1, MASKBYTE0, 0x0)?;
    debug_bb_read(bus, R_NCTL_RPT)?; // debug read: 0x8008
    Ok(fail)
}

// Linux: _iqk_one_shot (rtw8852b_rfk.c:815). Returns `fail`.
pub fn iqk_one_shot<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut RfkState,
    _phy_idx: u32,
    path: u32,
    ktype: IqkType,
) -> Rtw89Result<bool> {
    let p = path_idx(path)?;
    let pb = 1u32 << (4 + path);
    let bw = st.iqk.iqk_bw[p];
    let cmd = match ktype {
        IqkType::FlokCoarse => {
            phy_mask32(bus, R_P0_RFCTM, B_P0_RFCTM_EN, 0x1)?;
            0x108 | pb
        }
        IqkType::FlokFine => {
            phy_mask32(bus, R_P0_RFCTM, B_P0_RFCTM_EN, 0x1)?;
            0x208 | pb
        }
        IqkType::FlokVbuffer => {
            phy_mask32(bus, R_P0_RFCTM, B_P0_RFCTM_EN, 0x1)?;
            0x308 | pb
        }
        IqkType::Txk => {
            phy_mask32(bus, R_P0_RFCTM, B_P0_RFCTM_EN, 0x0)?;
            0x008 | pb | ((0x8u32.wrapping_add(bw) & 0xf) << 8)
        }
        IqkType::RxAgc => 0x508 | pb | (path << 1),
        IqkType::Rxk => {
            phy_mask32(bus, R_P0_RFCTM, B_P0_RFCTM_EN, 0x1)?;
            0x008 | pb | ((0xbu32.wrapping_add(bw) & 0xf) << 8)
        }
        IqkType::NbTxk => {
            phy_mask32(bus, R_P0_RFCTM, B_P0_RFCTM_EN, 0x0)?;
            phy_mask32(bus, R_IQK_DIF4, B_IQK_DIF4_TXT, 0x011)?;
            0x408 | pb
        }
        IqkType::NbRxk => {
            phy_mask32(bus, R_P0_RFCTM, B_P0_RFCTM_EN, 0x1)?;
            phy_mask32(bus, R_IQK_DIF4, B_IQK_DIF4_RXT, 0x011)?;
            0x608 | pb
        }
        IqkType::TxAgc => return Ok(false),
    };
    phy_mask32(bus, R_NCTL_CFG, MASKDWORD, cmd + 1)?;
    bus.delay_us(1)?;
    let fail = iqk_check_cal(bus, st, path)?;
    phy_mask32(bus, R_P0_RFCTM, B_P0_RFCTM_EN, 0x0)?;
    Ok(fail)
}

// Linux: _iqk_rxk_setting (rtw8852b_rfk.c:792)
pub fn iqk_rxk_setting<B: Bus + ?Sized>(bus: &mut B, st: &RfkState, path: u32) -> Rtw89Result<()> {
    let p = path_idx(path)?;
    let (g2, g5) = (is_2g(st, p), is_5g(st, p));
    if !g2 && !g5 {
        return Ok(());
    }
    rf_w(bus, path, RR_MOD, RR_MOD_MASK, 0xc)?;
    rf_w(
        bus,
        path,
        RR_RXK,
        if g2 { RR_RXK_SEL2G } else { RR_RXK_SEL5G },
        0x1,
    )?;
    let tmp = rf_r(bus, path, RR_CFGCH, RFREG_MASK)?;
    rf_w(bus, path, RR_RSV4, RFREG_MASK, tmp)
}

// Linux: _rxk_group_sel (rtw8852b_rfk.c:870). Returns `kfail`.
pub fn rxk_group_sel<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut RfkState,
    phy_idx: u32,
    path: u32,
) -> Rtw89Result<bool> {
    let p = path_idx(path)?;
    let lut = R_CFIR_LUT + (path << 8);
    let mut kfail = false;
    for gp in 0..RXK_GROUP_NR {
        let g = gp as usize;
        if is_2g(st, p) {
            rf_w(bus, path, RR_MOD, RR_MOD_RGM, t::G_IDXRXGAIN[g].into())?;
            rf_w(bus, path, RR_RXBB, RR_RXBB_C2G, t::G_IDXATTC2[g].into())?;
            rf_w(bus, path, RR_RXBB, RR_RXBB_C1G, t::G_IDXATTC1[g].into())?;
        } else if is_5g(st, p) {
            rf_w(bus, path, RR_MOD, RR_MOD_RGM, t::A_IDXRXGAIN[g].into())?;
            rf_w(bus, path, RR_RXA2, RR_RXA2_HATT, t::A_IDXATTC2[g].into())?;
            rf_w(bus, path, RR_RXA2, RR_RXA2_CC2, t::A_IDXATTC1[g].into())?;
        }
        phy_mask32(bus, lut, B_CFIR_LUT_SEL, 0x1)?;
        phy_mask32(bus, lut, B_CFIR_LUT_SET, 0x0)?;
        phy_mask32(bus, lut, B_CFIR_LUT_GP_V1, gp)?;
        let fail = iqk_one_shot(bus, st, phy_idx, path, IqkType::Rxk)?;
        phy_mask32(bus, R_IQKINF, 1 << (16 + gp + path * 4), u32::from(fail))?;
        kfail |= fail;
    }
    rf_w(bus, path, RR_RXK, RR_RXK_SEL5G, 0x0)?;
    let iqk = &mut st.iqk;
    iqk.nb_rxcfir[p] = if kfail { 0x4000_0002 } else { 0x4000_0000 };
    phy_mask32(
        bus,
        R_IQK_RES + (path << 8),
        B_IQK_RES_RXCFIR,
        if kfail { 0x0 } else { 0x5 },
    )?;
    iqk.is_wb_rxiqk[p] = !kfail;
    Ok(kfail)
}

// Linux: _iqk_rxclk_setting (rtw8852b_rfk.c:977): both branches differ only in the RX clock values
pub fn iqk_rxclk_setting<B: Bus + ?Sized>(
    bus: &mut B,
    st: &RfkState,
    path: u32,
) -> Rtw89Result<()> {
    let bw80 = st.iqk.iqk_bw[path_idx(path)?] == RTW89_CHANNEL_WIDTH_80;
    phy_mask32(bus, R_P0_NRBW, B_P0_NRBW_DBG, 0x1)?;
    phy_mask32(bus, R_P1_DBGMOD, B_P1_DBGMOD_ON, 0x1)?;
    bus.delay_us(1)?;
    phy_mask32(bus, R_ANAPAR_PW15, B_ANAPAR_PW15, 0x0f)?;
    bus.delay_us(1)?;
    phy_mask32(bus, R_ANAPAR_PW15, B_ANAPAR_PW15, 0x03)?;
    phy_mask32(bus, R_ANAPAR, B_ANAPAR_15, 0xa001)?;
    bus.delay_us(1)?;
    phy_mask32(bus, R_ANAPAR, B_ANAPAR_15, 0xa041)?;
    phy_mask32(bus, R_P0_RXCK, B_P0_RXCK_VAL, if bw80 { 0x2 } else { 0x1 })?;
    phy_mask32(bus, R_P0_RXCK, B_P0_RXCK_ON, 0x1)?;
    phy_mask32(bus, R_P1_RXCK, B_P1_RXCK_VAL, if bw80 { 0x2 } else { 0x1 })?;
    phy_mask32(bus, R_P1_RXCK, B_P1_RXCK_ON, 0x1)?;
    phy_mask32(bus, R_UPD_CLK_ADC, B_UPD_CLK_ADC_ON, 0x1)?;
    phy_mask32(bus, R_UPD_CLK_ADC, B_UPD_CLK_ADC_VAL, u32::from(bw80))
}

// Linux: _txk_group_sel (rtw8852b_rfk.c:1016). Returns `kfail`.
pub fn txk_group_sel<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut RfkState,
    phy_idx: u32,
    path: u32,
) -> Rtw89Result<bool> {
    let p = path_idx(path)?;
    let lut = R_CFIR_LUT + (path << 8);
    let mut kfail = false;
    for gp in 0..RXK_GROUP_NR {
        let g = gp as usize;
        let tbl = if is_2g(st, p) {
            Some((t::G_POWER_RANGE, t::G_TRACK_RANGE, t::G_GAIN_BB, t::G_ITQT))
        } else if is_5g(st, p) {
            Some((t::A_POWER_RANGE, t::A_TRACK_RANGE, t::A_GAIN_BB, t::A_ITQT))
        } else {
            None
        };
        if let Some((pr, tr, gb, itqt)) = tbl {
            rf_w(bus, path, RR_TXIG, RR_TXIG_GR0, pr[g].into())?;
            rf_w(bus, path, RR_TXIG, RR_TXIG_GR1, tr[g].into())?;
            rf_w(bus, path, RR_TXIG, RR_TXIG_TG, gb[g].into())?;
            phy_mask32(bus, R_KIP_IQP + (path << 8), MASKDWORD, itqt[g].into())?;
        }
        phy_mask32(bus, lut, B_CFIR_LUT_SEL, 0x1)?;
        phy_mask32(bus, lut, B_CFIR_LUT_SET, 0x1)?;
        phy_mask32(bus, lut, B_CFIR_LUT_G2, 0x0)?;
        phy_mask32(bus, lut, B_CFIR_LUT_GP, gp)?;
        phy_mask32(bus, R_NCTL_N1, B_NCTL_N1_CIP, 0x00)?;
        let fail = iqk_one_shot(bus, st, phy_idx, path, IqkType::Txk)?;
        phy_mask32(bus, R_IQKINF, 1 << (8 + gp + path * 4), u32::from(fail))?;
        kfail |= fail;
    }
    let iqk = &mut st.iqk;
    iqk.nb_txcfir[p] = if kfail { 0x4000_0002 } else { 0x4000_0000 };
    phy_mask32(
        bus,
        R_IQK_RES + (path << 8),
        B_IQK_RES_TXCFIR,
        if kfail { 0x0 } else { 0x5 },
    )?;
    iqk.is_wb_txiqk[p] = !kfail;
    Ok(kfail)
}

// Linux: _lok_res_table (rtw8852b_rfk.c:1127)
pub fn lok_res_table<B: Bus + ?Sized>(
    bus: &mut B,
    st: &RfkState,
    path: u32,
    ibias: u32,
) -> Rtw89Result<()> {
    let p = path_idx(path)?;
    rf_w(bus, path, RR_LUTWE, RFREG_MASK, 0x2)?;
    rf_w(
        bus,
        path,
        RR_LUTWA,
        RFREG_MASK,
        if is_2g(st, p) { 0x0 } else { 0x1 },
    )?;
    rf_w(bus, path, RR_LUTWD0, RFREG_MASK, ibias)?;
    rf_w(bus, path, RR_LUTWE, RFREG_MASK, 0x0)?;
    rf_w(bus, path, RR_TXVBUF, RR_TXVBUF_DACEN, 0x1)?;
    debug_rf_read(bus, path, RR_TXVBUF) // debug read: RF 0x7c
}

// Linux: _lok_finetune_check (rtw8852b_rfk.c:1147): the core and vbuffer codes must stay off
// the rails. Returns `fail`.
pub fn lok_finetune_check<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut RfkState,
    path: u32,
) -> Rtw89Result<bool> {
    let p = path_idx(path)?;
    let ch = st.mcc.slot()?;
    let tmp = rf_r(bus, path, RR_TXMO, RFREG_MASK)?;
    let (core_i, core_q) = (field(tmp, RR_TXMO_COI), field(tmp, RR_TXMO_COQ));
    let fail1 = !(0x2..=0x1d).contains(&core_i) || !(0x2..=0x1d).contains(&core_q);
    st.iqk.lok_idac[ch][p] = tmp;
    let tmp = rf_r(bus, path, RR_LOKVB, RFREG_MASK)?;
    let (vbuff_i, vbuff_q) = (field(tmp, RR_LOKVB_COI), field(tmp, RR_LOKVB_COQ));
    let fail2 = !(0x2..=0x3d).contains(&vbuff_i) || !(0x2..=0x3d).contains(&vbuff_q);
    st.iqk.lok_vbuf[ch][p] = tmp;
    Ok(fail1 || fail2)
}

// Linux: _iqk_lok (rtw8852b_rfk.c:1191); 2 GHz and 5 GHz differ only in TXIG_GR1. Returns
// `fail` of the finetune check.
pub fn iqk_lok<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut RfkState,
    phy_idx: u32,
    path: u32,
) -> Rtw89Result<bool> {
    let p = path_idx(path)?;
    let g2 = is_2g(st, p);
    let band = g2 || is_5g(st, p);
    let kip = R_KIP_IQP + (path << 8);
    let tg = |bus: &mut B, v: u32| -> Rtw89Result<()> {
        if band {
            rf_w(bus, path, RR_TXIG, RR_TXIG_TG, v)?;
        }
        Ok(())
    };
    phy_mask32(bus, R_IQK_DIF4, B_IQK_DIF4_TXT, 0x021)?;
    if band {
        rf_w(bus, path, RR_TXIG, RR_TXIG_GR0, 0x0)?;
        rf_w(bus, path, RR_TXIG, RR_TXIG_GR1, if g2 { 0x6 } else { 0x4 })?;
    }
    tg(bus, 0x0)?;
    phy_mask32(bus, kip, MASKDWORD, 0x9)?;
    st.iqk.lok_cor_fail[0][p] = iqk_one_shot(bus, st, phy_idx, path, IqkType::FlokCoarse)?;
    tg(bus, 0x12)?;
    phy_mask32(bus, kip, MASKDWORD, 0x24)?;
    iqk_one_shot(bus, st, phy_idx, path, IqkType::FlokVbuffer)?;
    tg(bus, 0x0)?;
    phy_mask32(bus, kip, MASKDWORD, 0x9)?;
    phy_mask32(bus, R_IQK_DIF4, B_IQK_DIF4_TXT, 0x021)?;
    st.iqk.lok_fin_fail[0][p] = iqk_one_shot(bus, st, phy_idx, path, IqkType::FlokFine)?;
    tg(bus, 0x12)?;
    phy_mask32(bus, kip, MASKDWORD, 0x24)?;
    iqk_one_shot(bus, st, phy_idx, path, IqkType::FlokVbuffer)?;
    lok_finetune_check(bus, st, path)
}

// Linux: _iqk_txk_setting (rtw8852b_rfk.c:1273)
pub fn iqk_txk_setting<B: Bus + ?Sized>(bus: &mut B, st: &RfkState, path: u32) -> Rtw89Result<()> {
    let p = path_idx(path)?;
    let g2 = is_2g(st, p);
    if g2 {
        rf_w(bus, path, RR_XALNA2, RR_XALNA2_SW2, 0x00)?;
        rf_w(bus, path, RR_TXG1, RR_TXG1_ATT2, 0x0)?;
        rf_w(bus, path, RR_TXG1, RR_TXG1_ATT1, 0x0)?;
        rf_w(bus, path, RR_TXG2, RR_TXG2_ATT0, 0x1)?;
    } else if is_5g(st, p) {
        rf_w(bus, path, RR_XGLNA2, RR_XGLNA2_SW, 0x00)?;
        rf_w(bus, path, RR_BIASA, RR_BIASA_A, 0x1)?;
    } else {
        return Ok(());
    }
    rf_w(bus, path, RR_TXGA, RR_TXGA_LOK_EXT, 0x0)?;
    rf_w(bus, path, RR_LUTWE, RR_LUTWE_LOK, 0x1)?;
    rf_w(
        bus,
        path,
        RR_LUTWA,
        RR_LUTWA_M1,
        if g2 { 0x00 } else { 0x80 },
    )?;
    rf_w(bus, path, RR_MOD, RR_MOD_IQK, 0x403e)?;
    bus.delay_us(1)?;
    Ok(())
}

// Linux: _iqk_txclk_setting (rtw8852b_rfk.c:1303)
pub fn iqk_txclk_setting<B: Bus + ?Sized>(bus: &mut B, _path: u32) -> Rtw89Result<()> {
    phy_mask32(bus, R_P0_NRBW, B_P0_NRBW_DBG, 0x1)?;
    phy_mask32(bus, R_P1_DBGMOD, B_P1_DBGMOD_ON, 0x1)?;
    bus.delay_us(1)?;
    phy_mask32(bus, R_ANAPAR_PW15, B_ANAPAR_PW15, 0x1f)?;
    bus.delay_us(1)?;
    phy_mask32(bus, R_ANAPAR_PW15, B_ANAPAR_PW15, 0x13)?;
    phy_mask32(bus, R_ANAPAR, B_ANAPAR_15, 0x0001)?;
    bus.delay_us(1)?;
    phy_mask32(bus, R_ANAPAR, B_ANAPAR_15, 0x0041)
}
