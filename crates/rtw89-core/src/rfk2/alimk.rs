//! The TSSI alignment calibration of Linux rtw8852b_rfk.c 3414-3737 (JS js/rfk2-alimk.mjs):
//! the PMAC sends test packets at two powers per path, the TSSI code word reports give the
//! alignment offsets, which are kept per band and per channel for later reloads. The PMAC/BB
//! helpers of rtw8852b_common.c are phy's (`phy::bbx`).

use alloc::vec::Vec;

use crate::bus::{Bus, prep};
use crate::error::Rtw89Result;
use crate::phy::Phy;
use crate::phy::bbx::{
    bb_backup_tssi, bb_cfg_tx_path, bb_restore_tssi, bb_set_plcp_tx, bb_set_pmac_pkt_tx,
    bb_set_power, bb_tx_mode_switch, ctrl_rx_path,
};
use crate::phy::io::{MASKDWORD, phy_mask32, phy_r32, phy_r32_mask};
use crate::rfk1::util::RfkChan;
use crate::tables::regs::*;
use crate::tables::rfk2::{
    ALIM_BB_REG, POWER, TSSI_CW_DEFAULT_ADDR, TSSI_CW_DEFAULT_MASK, TSSI_CW_RPT_ADDR, TSSI_TRIGGER,
};

use super::tssi::{alim_regs, tssi_alimk_band};
use super::{ALIMK_VALUES, Rfk2Event, Rfk2State, Steps, band_of, efuse_of, sign_extend32};

/// RTW8852B_TSSI_PATH_NR
const TSSI_PATH_NR: usize = 2;
/// `_tssi_get_cw_report`'s tries for a report
const CW_RETRY: u32 = 100;

// Linux: _tssi_hw_tx (rtw8852b_rfk.c:3414); `path` an enum rtw89_rf_path
#[allow(clippy::too_many_arguments)]
fn tssi_hw_tx<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &Phy,
    path: u32,
    cnt: u32,
    period: u32,
    pwr_dbm: u32,
    enable: bool,
    c: &RfkChan,
) -> Rtw89Result<()> {
    let rx_path = match path {
        RF_PATH_A => RF_A,
        RF_PATH_B => RF_B,
        RF_PATH_AB => RF_AB,
        _ => RF_ABCD, // don't change path, but still set others
    };
    if enable {
        let ef = efuse_of(phy)?;
        bb_set_plcp_tx(bus)?;
        bb_cfg_tx_path(bus, path)?;
        ctrl_rx_path(
            bus,
            rx_path,
            band_of(c),
            c.subband_type,
            &ef.gain,
            phy.hal.antenna_rx,
        )?;
        bb_set_power(bus, pwr_dbm as i32)?;
    }
    bb_set_pmac_pkt_tx(bus, enable, cnt, period, band_of(c))
}

// Linux: _tssi_backup_bb_registers (rtw8852b_rfk.c:3439)
fn tssi_backup_bb_registers<B: Bus + ?Sized>(bus: &mut B, reg: &[u32]) -> Rtw89Result<Vec<u32>> {
    reg.iter().map(|&a| phy_r32(bus, a)).collect()
}

// Linux: _tssi_reload_bb_registers (rtw8852b_rfk.c:3454)
fn tssi_reload_bb_registers<B: Bus + ?Sized>(
    bus: &mut B,
    reg: &[u32],
    backup: &[u32],
) -> Rtw89Result<()> {
    for (&a, &v) in reg.iter().zip(backup) {
        phy_mask32(bus, a, MASKDWORD, v)?;
    }
    Ok(())
}

// Linux: _tssi_ch_to_idx (rtw8852b_rfk.c:3470)
pub fn tssi_ch_to_idx(ch: u32) -> usize {
    let i = if (1..=14).contains(&ch) {
        ch - 1
    } else if (36..=64).contains(&ch) {
        (ch - 36) / 2 + 14
    } else if (100..=144).contains(&ch) {
        (ch - 100) / 2 + 15 + 14
    } else if (149..=177).contains(&ch) {
        (ch - 149) / 2 + 38 + 14
    } else {
        0
    };
    i as usize
}

// Linux: _tssi_get_cw_report (rtw8852b_rfk.c:3488): None when a report never came (100 tries
// 30 us apart), else the two code words. The TX counter reads are only logged in Linux.
fn tssi_get_cw_report<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &Phy,
    st: &mut Rfk2State,
    path: u32,
    power: &[u32],
    c: &RfkChan,
) -> Rtw89Result<Option<[u32; 2]>> {
    let p = path as usize;
    let mut rpt = [0u32; 2];
    for (j, r) in rpt.iter_mut().enumerate().take(TSSI_PATH_NR) {
        let pw = power.get(j).copied().unwrap_or(0);
        phy_mask32(bus, TSSI_TRIGGER[p], B_P0_TSSI_EN, 0x0)?;
        phy_mask32(bus, TSSI_TRIGGER[p], B_P0_TSSI_EN, 0x1)?;
        phy_r32(bus, R_TX_COUNTER)?; // tx_counter, then the trigger register only for the log
        phy_r32(bus, TSSI_TRIGGER[p])?;
        let hw_path = if j == 0 { path } else { RF_PATH_ABCD };
        tssi_hw_tx(bus, phy, hw_path, 100, 5000, pw, true, c)?;
        phy_r32_mask(bus, R_TX_COUNTER, MASKLWORD)?; // only logged
        let mut k = 0;
        while k < CW_RETRY {
            if phy_r32_mask(bus, TSSI_CW_RPT_ADDR[p], B_TSSI_CWRPT_RDY)? != 0 {
                break;
            }
            bus.delay_us(30)?;
            phy_r32_mask(bus, R_TX_COUNTER, MASKLWORD)?; // only logged
            k += 1;
        }
        if k >= CW_RETRY {
            st.push(Rfk2Event::TssiNoReport { path, power: pw });
            tssi_hw_tx(bus, phy, path, 100, 5000, pw, false, c)?;
            return Ok(None);
        }
        *r = phy_r32_mask(bus, TSSI_CW_RPT_ADDR[p], B_TSSI_CWRPT)?;
        tssi_hw_tx(bus, phy, path, 100, 5000, pw, false, c)?;
        phy_r32_mask(bus, R_TX_COUNTER, MASKLWORD)?; // only logged
    }
    Ok(Some(rpt))
}

// Linux: _tssi_alimentk (rtw8852b_rfk.c:3563); the dumps for the log and the time accounting
// are left out. Not Linux: when a step fails after the BB backup, the PMAC's test packets are
// switched off and the saved BB registers put back (best effort) before the error goes up.
pub fn tssi_alimentk<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &Phy,
    st: &mut Rfk2State,
    path: u32,
    c: &RfkChan,
) -> Rtw89Result<()> {
    let p = crate::rfk1::path_idx(path)?;
    let ch_idx = tssi_ch_to_idx(c.channel);
    let regs = alim_regs(path);
    if st.tssi.check_backup_aligmk[p][ch_idx] {
        let v = st.tssi.alignment_backup_by_ch[p][ch_idx];
        for (a, v) in regs.into_iter().zip(v) {
            phy_mask32(bus, a, MASKDWORD, v)?;
        }
        return Ok(());
    }
    // POWER_2G and POWER_5G are the same list
    let tssi_bak = bb_backup_tssi(bus)?;
    let bb_reg_backup = tssi_backup_bb_registers(bus, ALIM_BB_REG)?;
    let r = alimentk_measure(bus, phy, st, path, c);
    let failed = r.is_err();
    let mut steps = Steps::after(st, r);
    if failed {
        // Not Linux (its steps cannot fail): the PMAC may still be sending test packets
        steps.step(bus, "tssi pmac tx off", |b| {
            bb_set_pmac_pkt_tx(b, false, 100, 5000, band_of(c))
        });
    }
    steps.step(bus, "tssi bb reload", |b| {
        tssi_reload_bb_registers(b, ALIM_BB_REG, &bb_reg_backup)
    });
    steps.step(bus, "tssi bb restore", |b| bb_restore_tssi(b, &tssi_bak));
    steps.step(bus, "tssi tx mode", |b| bb_tx_mode_switch(b, 0));
    steps.finish()
}

// _tssi_alimentk between the BB backup and its reload: the test packets and the offsets
fn alimentk_measure<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &Phy,
    st: &mut Rfk2State,
    path: u32,
    c: &RfkChan,
) -> Rtw89Result<()> {
    let p = path as usize; // tssi_alimentk checked it
    let ch_idx = tssi_ch_to_idx(c.channel);
    let regs = alim_regs(path);
    let band = tssi_alimk_band(c.channel) as usize;
    phy_mask32(bus, R_P0_TSSI_AVG, B_P0_TSSI_AVG, 0x8)?;
    phy_mask32(bus, R_P1_TSSI_AVG, B_P1_TSSI_AVG, 0x8)?;
    phy_mask32(bus, R_P0_TSSI_MV_AVG, B_P0_TSSI_MV_AVG, 0x2)?;
    phy_mask32(bus, R_P1_TSSI_MV_AVG, B_P1_TSSI_MV_AVG, 0x2)?;

    if let Some(rpt) = tssi_get_cw_report(bus, phy, st, path, POWER, c)? {
        let cw_default = |b: &mut B, n: usize| -> Rtw89Result<i32> {
            let v = phy_r32_mask(b, TSSI_CW_DEFAULT_ADDR[p][n], TSSI_CW_DEFAULT_MASK[n])?;
            Ok(sign_extend32(v, 8))
        };
        let pdiff = (POWER[0] as i32 - POWER[1] as i32) * 2;
        let def = cw_default(bus, 1)?;
        let off1 = (rpt[0] as i32)
            .wrapping_sub(pdiff)
            .wrapping_sub(rpt[1] as i32)
            .wrapping_add(def);
        let diff = off1.wrapping_sub(def);
        let off2 = cw_default(bus, 2)?.wrapping_add(diff);
        let off3 = cw_default(bus, 3)?.wrapping_add(diff);
        let tmp = prep(B_P1_TSSI_ALIM11, off1 as u32)
            | prep(B_P1_TSSI_ALIM12, off2 as u32)
            | prep(B_P1_TSSI_ALIM13, off3 as u32);
        if p == 0 {
            phy_mask32(bus, R_P0_TSSI_ALIM1, B_P0_TSSI_ALIM1, tmp)?;
            phy_mask32(bus, R_P0_TSSI_ALIM2, B_P0_TSSI_ALIM2, tmp)?;
        } else {
            phy_mask32(bus, R_P1_TSSI_ALIM1, B_P1_TSSI_ALIM1, tmp)?;
            phy_mask32(bus, R_P1_TSSI_ALIM2, B_P1_TSSI_ALIM2, tmp)?;
        }
        // alignment_value[band], then alignment_backup_by_ch[ch]: the same four registers twice
        let mut v = [0u32; ALIMK_VALUES];
        for (x, &a) in v.iter_mut().zip(&regs) {
            *x = phy_r32(bus, a)?;
        }
        let mut w = [0u32; ALIMK_VALUES];
        for (x, &a) in w.iter_mut().zip(&regs) {
            *x = phy_r32(bus, a)?;
        }
        st.tssi.alignment_done[p][band] = true;
        st.tssi.alignment_value[p][band] = v;
        st.tssi.check_backup_aligmk[p][ch_idx] = true;
        st.tssi.alignment_backup_by_ch[p][ch_idx] = w;
    }
    Ok(())
}
