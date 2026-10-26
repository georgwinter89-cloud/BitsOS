//! The PHY and MAC helpers the station path calls in Linux (JS js/sta-ra.mjs): phy.c's rate
//! adaptation for the AP's link, the RSSI averages of the AP's link, DIG pause, EDCCA during
//! scans, the UL TB read at association; pci.c's RX interrupt mitigation; mac80211.c's EDCA value.

use crate::bus::{Bus, prep};
use crate::error::Rtw89Result;
use crate::phy::Phy;
use crate::phy::io::{phy_mask32, phy_r32_mask};
use crate::tables::regs::*;
use crate::txrx::PhyPpdu;

use super::{ApLink, EdcaParams, Vif};

/// DECLARE_EWMA(rssi, 10, 16) (core.h:4559; include/linux/average.h): precision 10, weight 1/16
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Ewma {
    pub internal: u32,
}

impl Ewma {
    // Linux: ewma_rssi_add of DECLARE_EWMA(rssi, 10, 16) (core.h:4559)
    pub fn add(&mut self, val: u32) {
        self.internal = if self.internal != 0 {
            ((self.internal.wrapping_mul(15)).wrapping_add(val << 10)) >> 4
        } else {
            val << 10
        };
    }
    // Linux: ewma_rssi_read of DECLARE_EWMA(rssi, 10, 16) (core.h:4559)
    pub fn read(&self) -> u32 {
        self.internal >> 10
    }
}

// Linux: rtw89_core_rx_process_phy_ppdu_iter (core.c:2035) for the one station, the AP's link
// (PHY 0): avg_rssi and rssi[] of both paths (no antenna diversity); the SNR/EVM averages only
// feed statistics
pub fn phy_ppdu_iter(ap: Option<&mut ApLink>, ppdu: &PhyPpdu) {
    let Some(ap) = ap else { return };
    if ppdu.phy_idx != 0 || ap.mac_id != ppdu.mac_id || !ppdu.to_self {
        return;
    }
    ap.avg_rssi.add(ppdu.rssi_avg.into());
    for (i, e) in ap.rssi.iter_mut().enumerate() {
        e.add(ppdu.rssi[i].into());
    }
}

// Linux: rtw89_phy_ra_mask_rssi (phy.c:158)
pub fn ra_mask_rssi(rssi: u32, ratr_state: u32) -> u64 {
    let mut lv: [u32; 7] = [30, 44, 48, 52, 56, 60, 100]; // rssi_lv_t, RA_FLOOR_TABLE_SIZE
    let rssi = (rssi & 0xff) >> 1;
    let mut rssi_lv = 0;
    for (i, t) in lv.iter_mut().enumerate() {
        if i as u32 >= ratr_state {
            *t += 3; // RA_FLOOR_UP_GAP
        }
        if rssi < *t {
            rssi_lv = i;
            break;
        }
    }
    match rssi_lv {
        0 => u64::MAX,
        1 => 0xffff_ffff_ffff_fff0,
        2 => 0xffff_ffff_ffff_efe0,
        3 => 0xffff_ffff_ffff_cfc0,
        4 => 0xffff_ffff_ffff_8f80,
        _ => 0xffff_ffff_ffff_0f00,
    }
}

// Linux: rtw89_phy_ra_mask_recover (phy.c:190)
pub fn ra_mask_recover(mut mask: u64, bak: u64) -> u64 {
    let legacy = u64::from(RA_MASK_CCK_RATES | RA_MASK_OFDM_RATES);
    if mask & !legacy == 0 {
        mask |= bak & !legacy;
    }
    if mask == 0 {
        mask |= bak & legacy;
    }
    mask
}

/// struct rtw89_ra_info as rtw89_fw_h2c_ra reads it
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RaInfo {
    pub mode_ctrl: u32,
    pub bw_cap: u32,
    pub macid: u32,
    pub dcm_cap: u32,
    pub er_cap: u32,
    pub init_rate_lv: u32,
    pub upd_all: u32,
    pub en_sgi: u32,
    pub ldpc_cap: u32,
    pub stbc_cap: u32,
    pub ss_num: u32,
    pub giltf: u32,
    pub upd_bw_nss_mask: u32,
    pub upd_mask: u32,
    pub ra_mask: u64,
    pub fix_giltf_en: u32,
    pub fix_giltf: u32,
}

// Linux: rtw89_phy_ra_sta_update (phy.c:322) for a legacy (no HT/VHT/HE) AP, no rate pattern,
// no cfg mask (rtw89_phy_ra_mask_cfg returns all ones)
pub fn ra_sta_update(sta: &ApLink, band_2g: bool, tx_nss: u32) -> RaInfo {
    let mut mode = 0;
    let mut mask;
    if band_2g {
        mask = u64::from(sta.supp_rates);
        if sta.supp_rates & 0xf != 0 {
            mode |= RTW89_RA_MODE_CCK;
        }
        if sta.supp_rates & 0xff0 != 0 {
            mode |= RTW89_RA_MODE_OFDM;
        }
    } else {
        mask = u64::from(sta.supp_rates) << 4;
        mode |= RTW89_RA_MODE_OFDM;
    }
    let bak = mask;
    if mode & RTW89_RA_MODE_OFDM != 0 {
        mask &= u64::from(RA_MASK_OFDM_RATES | RA_MASK_SUBCCK_RATES);
    }
    if mode != RTW89_RA_MODE_CCK {
        mask &= ra_mask_rssi(sta.avg_rssi.read(), 0);
    }
    mask = ra_mask_recover(mask, bak);
    RaInfo {
        mode_ctrl: mode,
        bw_cap: RTW89_CHANNEL_WIDTH_20,
        macid: sta.mac_id,
        ss_num: sta.rx_nss.min(tx_nss).saturating_sub(1),
        ra_mask: mask,
        ..RaInfo::default()
    }
}

// Linux: rtw89_phy_ra_assoc (phy.c:731): rate level from the RSSI, then the RA H2C (the caller's)
pub fn ra_assoc(sta: &ApLink, band_2g: bool, tx_nss: u32) -> RaInfo {
    let mut ra = ra_sta_update(sta, band_2g, tx_nss);
    let rssi = sta.avg_rssi.read() >> RSSI_FACTOR;
    ra.init_rate_lv = if rssi > 40 {
        1
    } else if rssi > 20 {
        2
    } else if rssi > 1 {
        3
    } else {
        0
    };
    ra.upd_all = 1;
    ra
}

// Linux: rtw89_phy_dig_ctrl (phy.c:7850) for PHY 0 on bb->dig (phy's `Phy::dig`); no DM bits
// disabled (hal->disabled_dm_bitmap 0). 8852B dig_regs: seg0_pd_reg R_SEG0R_PD_V1 (rtw8852b.c:217)
pub fn dig_ctrl<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &mut Phy,
    total_sta_assoc: u32,
    pause: bool,
    restore: bool,
) -> Rtw89Result<()> {
    if phy.dig.pause_dig == pause {
        return Ok(());
    }
    let en_dig = !pause && total_sta_assoc > 0;
    let pd = if !pause && restore {
        phy.dig.bak_dig as u32
    } else {
        0
    };
    phy_mask32(bus, R_SEG0R_PD_V1, B_SEG0R_PD_LOWER_BOUND_MSK, pd)?;
    phy_mask32(
        bus,
        R_SEG0R_PD_V1,
        B_SEG0R_PD_SPATIAL_REUSE_EN_MSK_V1,
        u32::from(en_dig),
    )?;
    phy.dig.pause_dig = pause;
    Ok(())
}

/// bb->edcca_bak: the EDCCA levels a scan replaced
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EdccaBak {
    pub a: u32,
    pub p: u32,
    pub ppdu: u32,
}

// Linux: rtw89_phy_config_edcca (phy.c:8641); 8852B edcca_regs (rtw8852b.c:241)
pub fn phy_config_edcca<B: Bus + ?Sized>(
    bus: &mut B,
    bak: &mut EdccaBak,
    scan: bool,
) -> Rtw89Result<()> {
    let lvl = R_SEG0R_EDCCA_LVL_V1;
    let (ma, mp, mppdu) = (B_EDCCA_LVL_MSK0, B_EDCCA_LVL_MSK1, B_EDCCA_LVL_MSK3);
    if scan {
        bak.a = phy_r32_mask(bus, lvl, ma)?;
        bak.p = phy_r32_mask(bus, lvl, mp)?;
        bak.ppdu = phy_r32_mask(bus, lvl, mppdu)?;
        phy_mask32(bus, lvl, ma, EDCCA_MAX)?;
        phy_mask32(bus, lvl, mp, EDCCA_MAX)?;
        phy_mask32(bus, lvl, mppdu, EDCCA_MAX)?;
    } else {
        phy_mask32(bus, lvl, ma, bak.a)?;
        phy_mask32(bus, lvl, mp, bak.p)?;
        phy_mask32(bus, lvl, mppdu, bak.ppdu)?;
    }
    Ok(())
}

// Linux: rtw89_pci_recalc_int_mit (pci.c:4261); 8852BE mit_addr R_AX_INT_MIT_RX. BitsOS polls,
// the register is written as in Linux. `tfc`: rtwdev->stats' TX and RX traffic levels.
pub fn hci_recalc_int_mit<B: Bus + ?Sized>(
    bus: &mut B,
    scanning: bool,
    tfc: (u32, u32),
) -> Rtw89Result<()> {
    let high = RTW89_TFC_HIGH;
    let mut val = 0;
    if !scanning && (tfc.0 >= high || tfc.1 >= high) {
        val = B_AX_RXMIT_RXP2_SEL
            | B_AX_RXMIT_RXP1_SEL
            | prep(B_AX_RXCOUNTER_MATCH_MASK, RTW89_PCI_RXBD_NUM_MAX / 2)
            | prep(B_AX_RXTIMER_UNIT_MASK, AX_RXTIMER_UNIT_64US)
            | prep(B_AX_RXTIMER_MATCH_MASK, 2048 / 64);
    }
    bus.w32(R_AX_INT_MIT_RX, val)?;
    Ok(())
}

// Linux: rtw89_phy_ul_tb_assoc (phy.c:5445); 8852B ul_tb_waveform_ctrl true, cv 1 not above
// CBV, 20 MHz: dyn_tb_bedge_en false
pub fn phy_ul_tb_assoc<B: Bus + ?Sized>(bus: &mut B, vif: &mut Vif) -> Rtw89Result<()> {
    vif.def_tri_idx = phy_r32_mask(bus, R_DCFO_OPT, B_TXSHAPE_TRIANGULAR_CFG)?;
    vif.dyn_tb_bedge_en = false;
    Ok(())
}

// Linux: rtw89_aifsn_to_aifs (mac80211.c:393)
pub fn aifsn_to_aifs(band_2g: bool, use_short_slot: bool, aifsn: u32) -> u32 {
    aifsn * if use_short_slot { 9 } else { 20 } + if band_2g { 10 } else { 16 }
}

// Linux: ____rtw89_conf_tx_edca (mac80211.c:414): the H2C value; params->txop in 32 us
pub fn conf_tx_edca_val(band_2g: bool, use_short_slot: bool, p: &EdcaParams) -> u32 {
    let ecw = |cw: u32| (cw + 1).checked_ilog2().unwrap_or(0);
    prep(FW_EDCA_PARAM_TXOPLMT_MSK, p.txop)
        | prep(FW_EDCA_PARAM_CWMAX_MSK, ecw(p.cw_max))
        | prep(FW_EDCA_PARAM_CWMIN_MSK, ecw(p.cw_min))
        | prep(
            FW_EDCA_PARAM_AIFS_MSK,
            aifsn_to_aifs(band_2g, use_short_slot, p.aifs),
        )
}

/// ac_to_fw_idx (mac80211.c:386): IEEE80211_AC_VO, _VI, _BE, _BK -> the firmware's AC
pub const AC_TO_FW_IDX: [u32; 4] = [3, 2, 0, 1];
