//! The baseband part of the RTL8852B channel switch (JS js/chan-bb.mjs; Linux
//! rtw8852b_common.c __rtw8852bx_set_channel_bb and its helpers) and the BB pieces of
//! rtw8852b.c set_channel_help. 8852B, PHY 0, dbcc off: the `_idx` accessors are the plain ones.
//! 20 MHz only, primary = center channel.

use crate::bus::{Bus, prep};
use crate::error::Rtw89Result;
use crate::phy::bbx::{bb_reset_all, set_gain_offset};
use crate::phy::io::{phy_mask32, phy_r32_mask, phy_w32};
use crate::phy::{Band, BbGain, Phy};
use crate::tables::chan::{CH_BASE, LNA_REGS, SCO_BARKER, SCO_CCK, TIA_REGS, TXFIR};
use crate::tables::regs::*;

use super::RtwChan;

// Linux: rtw8852bx_ctrl_sco_cck (rtw8852b_common.c:509), thresholds by primary channel - 1
fn ctrl_sco_cck<B: Bus + ?Sized>(bus: &mut B, primary_ch: u32) -> Rtw89Result<()> {
    let i = primary_ch.wrapping_sub(1) as usize;
    let at = |t: &[u32]| t.get(i).copied().unwrap_or(0);
    phy_mask32(bus, R_RXSCOBC, B_RXSCOBC_TH, at(SCO_BARKER))?;
    phy_mask32(bus, R_RXSCOCCK, B_RXSCOCCK_TH, at(SCO_CCK))
}

// Linux: rtw8852bx_sco_mapping (rtw8852b_common.c:519)
pub fn sco_mapping(ch: u32) -> u32 {
    match ch {
        1 => 109,
        2..=6 => 108,
        7..=10 => 107,
        11..=14 => 106,
        36 | 38 => 51,
        40..=58 => 50,
        60..=64 => 49,
        100 | 102 => 48,
        104..=126 => 47,
        128..=151 => 46,
        153..=177 => 45,
        _ => 0,
    }
}

// Linux: rtw89_subband_to_bb_gain_band (phy.h:832), 2 and 5 GHz
fn bb_gain_band(subband: u32) -> usize {
    (match subband {
        RTW89_CH_5G_BAND_1 => RTW89_BB_GAIN_BAND_5G_L,
        RTW89_CH_5G_BAND_3 => RTW89_BB_GAIN_BAND_5G_M,
        RTW89_CH_5G_BAND_4 => RTW89_BB_GAIN_BAND_5G_H,
        _ => RTW89_BB_GAIN_BAND_2G,
    }) as usize
}

// Linux: rtw8852bx_set_gain_error (rtw8852b_common.c:577): rtwdev->bb_gain.ax is phy's
// `Phy::bb_gain` ([gain_band][path][i], s8), filled by rtw89_phy_init_bb_reg
fn set_gain_error<B: Bus + ?Sized>(
    bus: &mut B,
    g: &BbGain,
    subband: u32,
    path: usize,
) -> Rtw89Result<()> {
    let gband = bb_gain_band(subband);
    let is2g = subband == RTW89_CH_2G;
    let s8 = |v: Option<&i8>| i32::from(v.copied().unwrap_or(0)) as u32;
    for (i, r) in LNA_REGS.iter().enumerate() {
        let reg = if is2g { r.g2[path] } else { r.g5[path] };
        let v = s8(g.lna_gain.get(gband).and_then(|p| p[path].get(i)));
        phy_mask32(bus, u32::from(reg), r.mask, v)?;
    }
    for (i, r) in TIA_REGS.iter().enumerate() {
        let reg = if is2g { r.g2[path] } else { r.g5[path] };
        let v = s8(g.tia_gain.get(gband).and_then(|p| p[path].get(i)));
        phy_mask32(bus, u32::from(reg), r.mask, v)?;
    }
    Ok(())
}

// Linux: rtw8852bx_set_rxsc_rpl_comp (rtw8852b_common.c:706): the averages of both paths' RPL
// offsets of the gain band
fn set_rxsc_rpl_comp<B: Bus + ?Sized>(bus: &mut B, g: &BbGain, subband: u32) -> Rtw89Result<()> {
    let b = bb_gain_band(subband);
    let pair = |t: Option<[i8; 2]>| {
        let [a, c] = t.unwrap_or([0, 0]);
        ((i32::from(a) + i32::from(c)) >> 1) as u32
    };
    let r20 = pair(g.rpl_ofst_20.get(b).map(|p| [p[0], p[1]]));
    let r40 = |i: usize| {
        pair(
            g.rpl_ofst_40
                .get(b)
                .and_then(|p| Some([*p[0].get(i)?, *p[1].get(i)?])),
        )
    };
    let r80 = |i: usize| {
        pair(
            g.rpl_ofst_80
                .get(b)
                .and_then(|p| Some([*p[0].get(i)?, *p[1].get(i)?])),
        )
    };
    let mut val = prep(B_P0_RPL1_20_MASK, r20)
        | prep(B_P0_RPL1_40_MASK, r40(0))
        | prep(B_P0_RPL1_41_MASK, r40(1));
    val >>= B_P0_RPL1_SHIFT;
    phy_mask32(bus, R_P0_RPL1, B_P0_RPL1_MASK as u32, val)?;
    phy_mask32(bus, R_P1_RPL1, B_P0_RPL1_MASK as u32, val)?;
    let val = prep(B_P0_RTL2_42_MASK, r40(2))
        | prep(B_P0_RTL2_80_MASK, r80(0))
        | prep(B_P0_RTL2_81_MASK, r80(1))
        | prep(B_P0_RTL2_8A_MASK, r80(10));
    phy_w32(bus, R_P0_RPL2, val)?;
    phy_w32(bus, R_P1_RPL2, val)?;
    let val = prep(B_P0_RTL3_82_MASK, r80(2))
        | prep(B_P0_RTL3_83_MASK, r80(3))
        | prep(B_P0_RTL3_84_MASK, r80(4))
        | prep(B_P0_RTL3_89_MASK, r80(9));
    phy_w32(bus, R_P0_RPL3, val)?;
    phy_w32(bus, R_P1_RPL3, val)
}

// Linux: rtw8852bx_ctrl_ch (rtw8852b_common.c:745): band select, SCO, the CCK TX FIR (written on
// 5 GHz too: only 6 GHz returns before it), gain error and offset, RPL compensation
fn ctrl_ch<B: Bus + ?Sized>(bus: &mut B, phy: &Phy, chan: &RtwChan) -> Rtw89Result<()> {
    let is2g = u32::from(chan.channel <= 14);
    phy_mask32(bus, R_PATH0_BAND_SEL_V1, B_PATH0_BAND_SEL_MSK_V1, is2g)?;
    phy_mask32(bus, R_PATH1_BAND_SEL_V1, B_PATH1_BAND_SEL_MSK_V1, is2g)?;
    phy_mask32(bus, R_FC0_BW_V1, B_FC0_BW_INV, sco_mapping(chan.channel))?;
    for f in TXFIR {
        let v = if chan.channel == 14 { f.ch14 } else { f.other };
        phy_mask32(bus, f.reg, f.mask, v)?;
    }
    set_gain_error(bus, &phy.bb_gain, chan.subband_type, 0)?;
    set_gain_error(bus, &phy.bb_gain, chan.subband_type, 1)?;
    let gain = phy
        .efuse
        .as_ref()
        .map(|e| e.gain.clone())
        .unwrap_or_default();
    set_gain_offset(bus, chan.subband_type, &gain, phy.hal.antenna_rx)?;
    set_rxsc_rpl_comp(bus, &phy.bb_gain, chan.subband_type)
}

// Linux: rtw8852b_bw_setting (rtw8852b_common.c:804), 20 MHz (40 and 80 MHz write the same)
fn bw_setting<B: Bus + ?Sized>(bus: &mut B, path: usize) -> Rtw89Result<()> {
    phy_mask32(bus, [0xC0EC, 0xC1EC][path], 0x6000, 0x0)?; // adc_sel
    phy_mask32(bus, [0xC0E4, 0xC1E4][path], 0x30, 0x2) // wbadc_sel
}

// Linux: rtw8852bx_ctrl_bw (rtw8852b_common.c:900), 20 MHz. 8852B: rtw8852b_bw_setting; the
// RTL8852BT NRBW and ADC parts do not apply.
fn ctrl_bw<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    let rx_path_0 = phy_r32_mask(bus, R_CHBW_MOD_V1, B_ANT_RX_SEG0)?;
    phy_mask32(bus, R_FC0_BW_V1, B_FC0_BW_SET, 0x0)?;
    phy_mask32(bus, R_CHBW_MOD_V1, B_CHBW_MOD_SBW, 0x0)?;
    phy_mask32(bus, R_CHBW_MOD_V1, B_CHBW_MOD_PRICH, 0x0)?;
    phy_mask32(bus, R_P0_RFMODE_ORI_RX, B_P0_RFMODE_ORI_RX_ALL, 0x333)?;
    phy_mask32(bus, R_P1_RFMODE_ORI_RX, B_P1_RFMODE_ORI_RX_ALL, 0x333)?;
    bw_setting(bus, 0)?;
    bw_setting(bus, 1)?;
    if rx_path_0 == 0x1 {
        phy_mask32(bus, R_P1_RFMODE_ORI_RX, B_P1_RFMODE_ORI_RX_ALL, 0x111)?;
    } else if rx_path_0 == 0x2 {
        phy_mask32(bus, R_P0_RFMODE_ORI_RX, B_P0_RFMODE_ORI_RX_ALL, 0x111)?;
    }
    Ok(())
}

// Linux: rtw8852bx_ctrl_cck_en (rtw8852b_common.c:1011)
fn ctrl_cck_en<B: Bus + ?Sized>(bus: &mut B, on: bool) -> Rtw89Result<()> {
    phy_mask32(bus, R_UPD_CLK_ADC, B_ENABLE_CCK, u32::from(on))?;
    phy_mask32(bus, R_RXCCA, B_RXCCA_DIS, u32::from(!on))
}

// Linux: rtw89_encode_chan_idx (phy.c:8582) with rtw89_ch_base_table (phy.c:8570), 2 and 5 GHz:
// 2 GHz the channel; 5 GHz the base index << 4 | (channel - base) / 2
pub fn encode_chan_idx(ch: u32, band: u32) -> u32 {
    if band == RTW89_BAND_2G {
        return ch & 0xf;
    }
    let mut idx = CH_BASE.len();
    while idx > 2 && ch < u32::from(CH_BASE[idx - 1]) {
        idx -= 1;
    }
    if idx <= 2 {
        return 0;
    }
    let i = idx - 1;
    (((i as u32) << 4) | ((ch.wrapping_sub(u32::from(CH_BASE[i])) >> 1) & 0xf)) & 0xff
}

// Linux: rtw8852bx_5m_mask (rtw8852b_common.c:1022), 20 MHz: masking off
fn mask_5m<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    phy_mask32(bus, R_PATH0_5MDET_V1, B_PATH0_5MDET_EN, 0x0)?;
    phy_mask32(bus, R_PATH1_5MDET_V1, B_PATH1_5MDET_EN, 0x0)?;
    phy_mask32(bus, R_ASSIGN_SBD_OPT_V1, B_ASSIGN_SBD_OPT_EN_V1, 0x0)
}

// Linux: __rtw8852bx_set_channel_bb (rtw8852b_common.c:1167), 8852B, 20 MHz. Not the RTL8852BT:
// no CSI tone index; the BT share block only on 5 GHz; rtw8852bx_bb_set_pop only in monitor mode.
pub fn set_channel_bb<B: Bus + ?Sized>(bus: &mut B, phy: &Phy, chan: &RtwChan) -> Rtw89Result<()> {
    let cck_en = chan.channel <= 14;
    if cck_en {
        ctrl_sco_cck(bus, chan.primary_channel)?;
    }
    ctrl_ch(bus, phy, chan)?;
    ctrl_bw(bus)?;
    ctrl_cck_en(bus, cck_en)?;
    if chan.band == Band::B5g {
        phy_mask32(bus, R_PATH0_BT_SHARE_V1, B_PATH0_BT_SHARE_V1, 0x0)?;
        phy_mask32(bus, R_PATH0_BTG_PATH_V1, B_PATH0_BTG_PATH_V1, 0x0)?;
        phy_mask32(bus, R_PATH1_BT_SHARE_V1, B_PATH1_BT_SHARE_V1, 0x0)?;
        phy_mask32(bus, R_PATH1_BTG_PATH_V1, B_PATH1_BTG_PATH_V1, 0x0)?;
        phy_mask32(bus, R_CHBW_MOD_V1, B_BT_SHARE, 0x0)?;
        phy_mask32(bus, R_FC0_BW_V1, B_ANT_RX_BT_SEG0, 0x0)?;
        phy_mask32(bus, R_BT_DYN_DC_EST_EN_V1, B_BT_DYN_DC_EST_EN_MSK, 0x0)?;
        phy_mask32(bus, R_GNT_BT_WGT_EN, B_GNT_BT_WGT_EN, 0x0)?;
    }
    let idx = encode_chan_idx(chan.primary_channel, chan.band_type());
    phy_mask32(bus, R_MAC_PIN_SEL, B_CH_IDX_SEG0, idx)?;
    mask_5m(bus)?;
    bb_reset_all(bus) // RTW89_PHY_0
}

// Linux: rtw8852b_tssi_cont_en_phyidx (rtw8852b.c:653) with rtw8852b_tssi_cont_en
// (rtw8852b.c:638); dbcc off: paths A and B. Path B uses the P0 masks, as the C does.
pub fn tssi_cont_en<B: Bus + ?Sized>(bus: &mut B, en: bool) -> Rtw89Result<()> {
    let v = u32::from(!en);
    for (rst, trk) in [
        (R_P0_TXPW_RSTB, R_P0_TSSI_TRK),
        (R_P1_TXPW_RSTB, R_P1_TSSI_TRK),
    ] {
        phy_mask32(bus, rst, B_P0_TXPW_RSTB_MANON, v)?;
        phy_mask32(bus, trk, B_P0_TSSI_TRK_EN, v)?;
    }
    Ok(())
}

// Linux: rtw8852b_adc_en (rtw8852b.c:667)
pub fn adc_en<B: Bus + ?Sized>(bus: &mut B, en: bool) -> Rtw89Result<()> {
    phy_mask32(bus, R_ADC_FIFO, B_ADC_FIFO_RST, if en { 0x0 } else { 0xf })
}
