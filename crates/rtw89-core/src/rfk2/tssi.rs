//! The TSSI (closed-loop TX power) set-up of Linux rtw8852b_rfk.c 2719-3412 (JS
//! js/rfk2-tssi.mjs): the RF and BB tables per path, the thermal meter table, enable/disable,
//! the efuse DE values per channel group and the stored alignment values. The alignment
//! calibration itself (PMAC test packets) is in `alimk`.

use crate::bus::Bus;
use crate::error::Rtw89Result;
use crate::phy::Phy;
use crate::phy::io::{MASKDWORD, phy_mask32, phy_w32, rf_w};
use crate::rfk1::util::{RF_PATH_NUM_8852B, RfkChan, rfk_parser};
use crate::tables::TssiGroup;
use crate::tables::regs::*;
use crate::tables::rfk2::*;
use crate::tables::trk::DELTA_SWINGIDX_SIZE;

use super::{Rfk2State, efuse_of, trk};

/// _TSSI_DE_MASK (rtw8852b_rfk.c): GENMASK(21, 12)
const TSSI_DE_MASK: u32 = 0x003f_f000;

fn is2g(c: &RfkChan) -> bool {
    c.band_type == RTW89_BAND_2G
}

/// The index of path `path` (0 or 1) into the per-path tables
fn pi(path: u32) -> Rtw89Result<usize> {
    crate::rfk1::path_idx(path)
}

// Linux: _tssi_rf_setting (rtw8852b_rfk.c:2719)
pub fn tssi_rf_setting<B: Bus + ?Sized>(bus: &mut B, path: u32, c: &RfkChan) -> Rtw89Result<()> {
    if is2g(c) {
        rf_w(bus, path, RR_TXPOW, RR_TXPOW_TXG, 0x1)
    } else {
        rf_w(bus, path, RR_TXPOW, RR_TXPOW_TXA, 0x1)
    }
}

// Linux: _tssi_set_sys (rtw8852b_rfk.c:2730)
pub fn tssi_set_sys<B: Bus + ?Sized>(bus: &mut B, path: u32, c: &RfkChan) -> Rtw89Result<()> {
    let p = pi(path)?;
    rfk_parser(bus, TSSI_SYS_DEFS)?;
    rfk_parser(bus, PICK_TSSI_SYS_PATH[p * 2 + usize::from(!is2g(c))])
}

// Linux: _tssi_ini_txpwr_ctrl_bb (rtw8852b_rfk.c:2747)
pub fn tssi_ini_txpwr_ctrl_bb<B: Bus + ?Sized>(bus: &mut B, path: u32) -> Rtw89Result<()> {
    rfk_parser(bus, PICK_TSSI_INIT_TXPWR[pi(path)?])
}

// Linux: _tssi_ini_txpwr_ctrl_bb_he_tb (rtw8852b_rfk.c:2756)
pub fn tssi_ini_txpwr_ctrl_bb_he_tb<B: Bus + ?Sized>(bus: &mut B, path: u32) -> Rtw89Result<()> {
    rfk_parser(bus, PICK_TSSI_INIT_TXPWR_HE_TB[pi(path)?])
}

// Linux: _tssi_set_dck (rtw8852b_rfk.c:2765)
pub fn tssi_set_dck<B: Bus + ?Sized>(bus: &mut B, path: u32) -> Rtw89Result<()> {
    rfk_parser(bus, PICK_TSSI_DCK[pi(path)?])
}

/// The registers `_tssi_set_tmeter_tbl` writes for one path
struct Tmeter {
    tm: u32,
    dis: u32,
    trk: u32,
    val: u32,
    ctm: u32,
    ctm_val: u32,
    base: u32,
    rdy: u32,
}

const TMETER: [Tmeter; 2] = [
    Tmeter {
        tm: R_P0_TMETER,
        dis: B_P0_TMETER_DIS,
        trk: B_P0_TMETER_TRK,
        val: B_P0_TMETER,
        ctm: R_P0_RFCTM,
        ctm_val: B_P0_RFCTM_VAL,
        base: R_P0_TSSI_BASE,
        rdy: R_P0_RFCTM_RDY, // Linux writes the R_ name as the mask
    },
    Tmeter {
        tm: R_P1_TMETER,
        dis: B_P1_TMETER_DIS,
        trk: B_P1_TMETER_TRK,
        val: B_P1_TMETER,
        ctm: R_P1_RFCTM,
        ctm_val: B_P1_RFCTM_VAL,
        base: R_TSSI_THOF,
        rdy: R_P1_RFCTM_RDY,
    },
];

// Linux: _tssi_set_tmeter_tbl (rtw8852b_rfk.c:2773). The thermal value of the efuse
// (rtwdev->tssi.thermal) comes from phy's efuse; the deltas from the TXPWR_TRK element or
// rtw89_8852b_trk_cfg (`trk`).
pub fn tssi_set_tmeter_tbl<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &Phy,
    st: &Rfk2State,
    path: u32,
    c: &RfkChan,
) -> Rtw89Result<()> {
    let p = pi(path)?;
    let a = p == 0;
    let (up_t, down_t, row) = match c.subband_type {
        RTW89_CH_5G_BAND_1 | RTW89_CH_5G_BAND_3 | RTW89_CH_5G_BAND_4 => {
            let row = match c.subband_type {
                RTW89_CH_5G_BAND_1 => 0,
                RTW89_CH_5G_BAND_3 => 1,
                _ => 2,
            };
            if a {
                (
                    RTW89_FW_TXPWR_TRK_TYPE_5GA_P,
                    RTW89_FW_TXPWR_TRK_TYPE_5GA_N,
                    row,
                )
            } else {
                (
                    RTW89_FW_TXPWR_TRK_TYPE_5GB_P,
                    RTW89_FW_TXPWR_TRK_TYPE_5GB_N,
                    row,
                )
            }
        }
        _ if a => (
            RTW89_FW_TXPWR_TRK_TYPE_2GA_P,
            RTW89_FW_TXPWR_TRK_TYPE_2GA_N,
            0,
        ),
        _ => (
            RTW89_FW_TXPWR_TRK_TYPE_2GB_P,
            RTW89_FW_TXPWR_TRK_TYPE_2GB_N,
            0,
        ),
    };
    let thm_up = trk::row(st, up_t, row)?;
    let thm_down = trk::row(st, down_t, row)?;
    let r = &TMETER[p];
    let thermal = efuse_of(phy)?.tssi.thermal[p];

    phy_mask32(bus, r.tm, r.dis, 0x0)?;
    phy_mask32(bus, r.tm, r.trk, 0x1)?;
    if thermal == 0xff {
        phy_mask32(bus, r.tm, r.val, 32)?;
        phy_mask32(bus, r.ctm, r.ctm_val, 32)?;
        for i in (0..64).step_by(4) {
            phy_w32(bus, r.base + i, 0x0)?;
        }
    } else {
        phy_mask32(bus, r.tm, r.val, thermal.into())?;
        phy_mask32(bus, r.ctm, r.ctm_val, thermal.into())?;
        let last = DELTA_SWINGIDX_SIZE - 1;
        let mut ofst = [0i8; 64];
        let mut i = 0usize;
        for o in ofst.iter_mut().take(32) {
            let v = if i < DELTA_SWINGIDX_SIZE {
                i += 1;
                thm_down[i - 1]
            } else {
                thm_down[last]
            };
            *o = v.wrapping_neg(); // the (s8) of -thm_down[]
        }
        i = 1;
        for j in (32..64).rev() {
            ofst[j] = if i < DELTA_SWINGIDX_SIZE {
                i += 1;
                thm_up[i - 1]
            } else {
                thm_up[last]
            };
        }
        for (k, q) in ofst.as_chunks::<4>().0.iter().enumerate() {
            // RTW8852B_TSSI_GET_VAL: four s8 as bytes, the first in bits 7:0
            let v = u32::from_le_bytes(q.map(|x| x as u8));
            phy_w32(bus, r.base + (k as u32) * 4, v)?;
        }
    }
    phy_mask32(bus, r.ctm, r.rdy, 0x1)?;
    phy_mask32(bus, r.ctm, r.rdy, 0x0)
}

// Linux: _tssi_set_dac_gain_tbl (rtw8852b_rfk.c:2934)
pub fn tssi_set_dac_gain_tbl<B: Bus + ?Sized>(bus: &mut B, path: u32) -> Rtw89Result<()> {
    rfk_parser(bus, PICK_TSSI_DAC_GAIN[pi(path)?])
}

// Linux: _tssi_slope_cal_org (rtw8852b_rfk.c:2942)
pub fn tssi_slope_cal_org<B: Bus + ?Sized>(bus: &mut B, path: u32, c: &RfkChan) -> Rtw89Result<()> {
    rfk_parser(
        bus,
        PICK_TSSI_SLOPE_ORG[pi(path)? * 2 + usize::from(!is2g(c))],
    )
}

// Linux: _tssi_alignment_default (rtw8852b_rfk.c:2957); path A tests the band, path B the
// channel number, as in C
pub fn tssi_alignment_default<B: Bus + ?Sized>(
    bus: &mut B,
    path: u32,
    all: bool,
    c: &RfkChan,
) -> Rtw89Result<()> {
    let p = pi(path)?;
    let ch = c.channel;
    let g2 = if p == 0 {
        is2g(c)
    } else {
        (1..=14).contains(&ch)
    };
    let part = if g2 {
        0
    } else if (36..=64).contains(&ch) {
        1
    } else if (100..=144).contains(&ch) {
        2
    } else if (149..=177).contains(&ch) {
        3
    } else {
        return Ok(());
    };
    rfk_parser(bus, PICK_TSSI_ALIGN[p * 8 + part * 2 + usize::from(!all)])
}

// Linux: _tssi_set_tssi_slope (rtw8852b_rfk.c:3015)
pub fn tssi_set_tssi_slope<B: Bus + ?Sized>(bus: &mut B, path: u32) -> Rtw89Result<()> {
    rfk_parser(bus, PICK_TSSI_SLOPE[pi(path)?])
}

// Linux: _tssi_enable (rtw8852b_rfk.c:3045) with _tssi_set_tssi_track (:3023) and
// _tssi_set_txagc_offset_mv_avg (:3032); sets rtwdev->is_tssi_mode
pub fn tssi_enable<B: Bus + ?Sized>(bus: &mut B, phy: &mut Phy) -> Rtw89Result<()> {
    for i in RF_PATH_A..RF_PATH_NUM_8852B {
        let a = i == RF_PATH_A;
        let pick = |x: u32, y: u32| if a { x } else { y };
        phy_mask32(
            bus,
            pick(R_P0_TSSIC, R_P1_TSSIC),
            pick(B_P0_TSSIC_BYPASS, B_P1_TSSIC_BYPASS),
            0x0,
        )?;
        // path B: the mask is B_P1_RFCTM_DEL in Linux (the same bits as B_P0_TSSI_MV_MIX)
        let mv = pick(R_P0_TSSI_MV_AVG, R_P1_TSSI_MV_AVG);
        phy_mask32(bus, mv, pick(B_P0_TSSI_MV_MIX, B_P1_RFCTM_DEL), 0x010)?;
        phy_mask32(bus, mv, pick(B_P0_TSSI_MV_CLR, B_P1_TSSI_MV_CLR), 0x0)?;
        let avg = pick(R_P0_TSSI_AVG, R_P1_TSSI_AVG);
        let en = pick(B_P0_TSSI_EN, B_P1_TSSI_EN);
        phy_mask32(bus, avg, en, 0x0)?;
        phy_mask32(bus, avg, en, 0x1)?;
        rf_w(bus, i, RR_TXGA_V1, RR_TXGA_V1_TRK_EN, 0x1)?;
        let t = pick(R_P0_TSSI_TRK, R_P1_TSSI_TRK);
        phy_mask32(bus, t, pick(B_P0_TSSI_RFC, B_P1_TSSI_RFC), 0x3)?;
        phy_mask32(bus, t, pick(B_P0_TSSI_OFT, B_P1_TSSI_OFT), 0xc0)?;
        let oft_en = pick(B_P0_TSSI_OFT_EN, B_P1_TSSI_OFT_EN);
        phy_mask32(bus, t, oft_en, 0x0)?;
        phy_mask32(bus, t, oft_en, 0x1)?;
        phy.tssi_mode[i as usize] = true;
    }
    Ok(())
}

// Linux: _tssi_disable (rtw8852b_rfk.c:3097)
pub fn tssi_disable<B: Bus + ?Sized>(bus: &mut B, phy: &mut Phy) -> Rtw89Result<()> {
    for (avg, en, t, rfc, mv, clr) in [
        (
            R_P0_TSSI_AVG,
            B_P0_TSSI_EN,
            R_P0_TSSI_TRK,
            B_P0_TSSI_RFC,
            R_P0_TSSI_MV_AVG,
            B_P0_TSSI_MV_CLR,
        ),
        (
            R_P1_TSSI_AVG,
            B_P1_TSSI_EN,
            R_P1_TSSI_TRK,
            B_P1_TSSI_RFC,
            R_P1_TSSI_MV_AVG,
            B_P1_TSSI_MV_CLR,
        ),
    ] {
        phy_mask32(bus, avg, en, 0x0)?;
        phy_mask32(bus, t, rfc, 0x1)?;
        phy_mask32(bus, mv, clr, 0x1)?;
    }
    phy.tssi_mode = [false; 2];
    Ok(())
}

/// `ch` in a group table: (group, extra); a channel in none is group 0 (Linux's default)
fn group(tbl: &[TssiGroup], ch: u32) -> (usize, bool) {
    tbl.iter()
        .find(|g| (u32::from(g.first)..=u32::from(g.last)).contains(&ch))
        .map_or((0, false), |g| (g.group.into(), g.extra))
}

// Linux: _tssi_get_ofdm_de (rtw8852b_rfk.c:3228) with _tssi_get_ofdm_group (:3136): an extra
// group (between two) is the truncated average of its neighbours
fn tssi_get_ofdm_de(mcs: &[i8], ch: u32) -> i32 {
    let (g, extra) = group(OFDM_GROUPS, ch);
    let at = |i: usize| i32::from(mcs.get(i).copied().unwrap_or(0));
    if extra {
        super::s8((at(g) + at(g + 1)) / 2)
    } else {
        at(g)
    }
}

// Linux: _tssi_get_ofdm_trim_de (rtw8852b_rfk.c:3263) with _tssi_get_trim_group (:3204); a
// phycap map never read leaves the trim 0, as Linux's zeroed rtwdev
fn tssi_get_ofdm_trim_de(trim: &[i8], ch: u32) -> i32 {
    let (g, _) = group(TRIM_GROUPS, ch);
    i32::from(trim.get(g).copied().unwrap_or(0))
}

// Linux: _tssi_set_efuse_to_de (rtw8852b_rfk.c:3300) with _tssi_get_cck_group (:3110); the
// reads for the log are left out
pub fn tssi_set_efuse_to_de<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &Phy,
    c: &RfkChan,
) -> Rtw89Result<()> {
    let t = &efuse_of(phy)?.tssi;
    let ch = c.channel;
    for i in 0..RF_PATH_NUM_8852B as usize {
        let trim = tssi_get_ofdm_trim_de(&t.tssi_trim[i], ch);
        let (gidx, _) = group(CCK_GROUPS, ch);
        let cck = i32::from(t.tssi_cck[i].get(gidx).copied().unwrap_or(0));
        let val = cck.wrapping_add(trim) as u32;
        phy_mask32(bus, DE_CCK_LONG[i], TSSI_DE_MASK, val)?;
        phy_mask32(bus, DE_CCK_SHORT[i], TSSI_DE_MASK, val)?;
        let val = tssi_get_ofdm_de(&t.tssi_mcs[i], ch).wrapping_add(trim) as u32;
        for de in [
            DE_MCS20M,
            DE_MCS40M,
            DE_MCS80M,
            DE_MCS80M80M,
            DE_MCS5M,
            DE_MCS10M,
        ] {
            phy_mask32(bus, de[i], TSSI_DE_MASK, val)?;
        }
    }
    Ok(())
}

// Linux: the TSSI_ALIMK_* band switch of rtw8852b_tssi_scan (rtw8852b_rfk.c:3867), the same in
// _tssi_alimentk_done (:3389) and _tssi_alimentk (:3613)
pub fn tssi_alimk_band(ch: u32) -> u32 {
    if (36..=64).contains(&ch) {
        TSSI_ALIMK_5GL
    } else if (100..=144).contains(&ch) {
        TSSI_ALIMK_5GM
    } else if (149..=177).contains(&ch) {
        TSSI_ALIMK_5GH
    } else {
        TSSI_ALIMK_2G
    }
}

/// The four alignment registers of a path in the order Linux writes and reads them
pub(crate) fn alim_regs(path: u32) -> [u32; 4] {
    let o = path << 13;
    [
        R_P0_TSSI_ALIM1 + o,
        R_P0_TSSI_ALIM3 + o,
        R_P0_TSSI_ALIM2 + o,
        R_P0_TSSI_ALIM4 + o,
    ]
}

// Linux: _tssi_alimentk_done (rtw8852b_rfk.c:3378); the register dump for the log is left out
pub fn tssi_alimentk_done<B: Bus + ?Sized>(
    bus: &mut B,
    st: &Rfk2State,
    path: u32,
    c: &RfkChan,
) -> Rtw89Result<()> {
    let p = pi(path)?;
    let band = tssi_alimk_band(c.channel) as usize;
    if !st.tssi.alignment_done[p][band] {
        return Ok(());
    }
    let v = st.tssi.alignment_value[p][band];
    for (a, v) in alim_regs(path).into_iter().zip(v) {
        phy_mask32(bus, a, MASKDWORD, v)?;
    }
    Ok(())
}
