//! `_action_common` (coex.c:6679) and its steps as a WLAN-only run on the RTL8852B reaches them
//! (JS js/btc-run.mjs): the WL TX power / RX gain (never changed: Linux passes RTW89_PHY_0 as a
//! bit map), the PTA's HW band, the ZB table, BTG and pre-AGC control, the TX limit, the BT RX AGC
//! and scan priority scoreboard bits, the RF TRX parameters, the firmware's report map, the role
//! H2C and the WL->BT scoreboard.

use crate::bus::Bus;
use crate::error::Rtw89Result;
use crate::tables::btc::{RF_DL, RF_UL, rsn, wlink, wscb};
use crate::tables::regs::*;

use super::c2h::read_pre_agc;
use super::h2c::{fw_en_rpt, h2c_cx_role};
use super::hw::{
    btc_ctrl_nbtg_bt_tx, btc_set_wl_rx_gain, btc_set_wl_txpwr_ctrl, btc_wl_s1_standby, mac_cfg_sb,
};
use super::run::write_scbd;
use super::{BtcHooks, BtcState, not_ported};

/// BTC_FDDT_DISABLE
const FDDT_DISABLE: u32 = 0;
/// rtw89_btc_fw_rpt_ver (coex.c:2847) for frptmap 3
const RPT_MREG: u32 = 1 << 2;
const RPT_BT_VER_INFO: u32 = 1 << 3;
const RPT_BT_SCAN_INFO: u32 = 1 << 4;
const RPT_BT_DEVICE_INFO: u32 = 1 << 5;
const RPT_BT_AFH_MAP: u32 = 1 << 6;
const RPT_BT_AFH_MAP_LE: u32 = 1 << 7;
const RPT_BT_TX_PWR_LVL: u32 = 0;
/// chip_id_to_bt_rom_code_id (coex.c:360)
const BT_ROM_CODE_8852: u32 = 0x8852;

// Linux: _set_wl_tx_power (coex.c:3769), fcxtrx 0. phy_map is a bit map and Linux passes
// RTW89_PHY_0 (0), so nothing changes unless debugfs set cli_h2c_cmd: kept.
pub fn set_wl_tx_power<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    level: u32,
    phy_map: u32,
) -> Rtw89Result<()> {
    let p = &mut st.dm.wl_tx_power;
    let mut chg = false;
    if phy_map & 1 != 0 {
        chg = p[0] != level;
    }
    if phy_map & 2 != 0 {
        chg |= p[1] != level;
    }
    if !chg {
        return Ok(());
    }
    if phy_map & 1 != 0 {
        p[0] = level;
    }
    if phy_map & 2 != 0 {
        p[1] = level;
    }
    if level != RTW89_BTC_WL_DEF_TX_PWR {
        return not_ported("forced WL TX power", level);
    }
    btc_set_wl_txpwr_ctrl(bus, 0xffff_ffff) // WL_TX_POWER_NO_BTC_CTRL
}

// Linux: _set_wl_rx_gain (coex.c:3821), the same phy_map rule
pub fn set_wl_rx_gain<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    level: u32,
    phy_map: u32,
) -> Rtw89Result<()> {
    let p = &mut st.dm.wl_rx_gain;
    let mut chg = false;
    if phy_map & 1 != 0 {
        chg = p[0] != level;
    }
    if phy_map & 2 != 0 {
        chg |= p[1] != level;
    }
    if !chg {
        return Ok(());
    }
    if phy_map & 1 != 0 {
        p[0] = level;
    }
    if phy_map & 2 != 0 {
        p[1] = level;
    }
    btc_set_wl_rx_gain(bus, st, level)
}

// Linux: _wl_req_mac (coex.c:6566), fcxosi 0: R_AX_BTC_CFG picks the PTA's HW band
fn wl_req_mac<B: Bus + ?Sized>(bus: &mut B, st: &mut BtcState, mac: u32) -> Rtw89Result<()> {
    let wl = &mut st.wl;
    if mac == wl.pta_req_mac {
        return Ok(());
    }
    wl.pta_req_mac = mac;
    wl.pta_reg_mac_chg = true;
    if mac == RTW89_MAC_0 {
        bus.clr32(R_AX_BTC_CFG, B_AX_WL_SRC)?;
    } else {
        bus.set32(R_AX_BTC_CFG, B_AX_WL_SRC)?;
    }
    Ok(())
}

// Linux: _update_zb_coex_tbl (coex.c:6594); the 8852B's wa_type 0 ends it
fn update_zb_coex_tbl<B: Bus + ?Sized>(bus: &mut B, st: &BtcState) -> Rtw89Result<()> {
    let dm = &st.dm;
    if dm.run_reason != rsn::NTFY_INIT && st.wl.link_mode_chg == 0 {
        return Ok(());
    }
    if st.md.wa_type & BTC_WA_HFP_ZB == 0 {
        return Ok(());
    }
    let (mut t0, mut t1) = (0xda5a_5a5a, 0xda5a_5a5a);
    if dm.tdd_bind.rf_band == (1 << RTW89_BAND_5G) || dm.freerun {
        (t0, t1) = (0xffff_ffff, 0xffff_ffff);
    } else if st.wl.role.link_mode == wlink::DB_MCC {
        t0 = 0xffff_ffff;
    }
    bus.w32(R_BTC_ZB_COEX_TBL_0, t0)?;
    bus.w32(R_BTC_ZB_COEX_TBL_1, t1)?;
    Ok(())
}

// Linux: _set_btg_ctrl (coex.c:6190): shared antenna, BT not running -> BTC_BTGCTRL_DISABLE,
// the reset value, so nothing is written
fn set_btg_ctrl(st: &mut BtcState) -> Rtw89Result<()> {
    let mut is_btg = BTC_BTGCTRL_DISABLE;
    if st.bt0.run_patch_code != 0 && st.bt0.enable_now != 0 {
        return not_ported("BTG RX control", st.bt0.scbd);
    }
    if st.wl.role.link_mode == wlink::DB_MCC {
        is_btg = BTC_BTGCTRL_BB_GNT_FWCTRL;
    }
    if is_btg == st.dm.wl_btg_rx {
        return Ok(());
    }
    st.dm.wl_btg_rx = is_btg;
    if is_btg > BTC_BTGCTRL_ENABLE {
        return Ok(()); // set by the WL firmware
    }
    not_ported("BTG RX control (ctrl_btg_bt_rx)", is_btg)
}

// Linux: _set_wl_preagc_ctrl (coex.c:6257): BT not running -> disabled. Until the firmware's first
// MREG report the readback is NOTFOUND and the BB table is written on every run; after it, only on
// INIT and SWBAND runs or when the readback differs (ost_info.rf_band S0 = S1 = 0: both paths get
// the same value).
fn set_wl_preagc_ctrl<B: Bus + ?Sized>(bus: &mut B, st: &mut BtcState) -> Rtw89Result<()> {
    let mut pre = BTC_PREAGC_DISABLE;
    if st.bt0.run_patch_code != 0 && st.bt0.enable_now != 0 {
        return not_ported("pre-AGC with BT on", st.bt0.scbd);
    }
    if st.wl.role.link_mode == wlink::DB_MCC {
        pre = BTC_PREAGC_BB_FWCTRL;
    }
    let dm = &st.dm;
    if dm.wl_pre_agc_rb != dm.wl_pre_agc && dm.wl_pre_agc_rb != BTC_PREAGC_NOTFOUND {
        st.dm.wl_pre_agc_rb = read_pre_agc(bus, st)?;
    }
    let dm = &st.dm;
    let normal = st.wl.coex_mode == BTC_MODE_NORMAL;
    let rr = dm.run_reason;
    if (normal
        && (rr == rsn::NTFY_INIT || rr == rsn::NTFY_SWBAND || dm.wl_pre_agc_rb != dm.wl_pre_agc))
        || pre != dm.wl_pre_agc
    {
        st.dm.wl_pre_agc = pre;
        if pre > BTC_PREAGC_ENABLE {
            return Ok(());
        }
        btc_ctrl_nbtg_bt_tx(bus, pre == BTC_PREAGC_ENABLE)?; // chip->ops->ctrl_nbtg_bt_tx, PHY 0
    }
    Ok(())
}

// Linux: _set_wl_tx_limit (coex.c:6417): no BT link -> the default limits, which are the reset
// values, so no station is touched
fn set_wl_tx_limit(st: &BtcState) -> Rtw89Result<()> {
    let dm = &st.dm;
    if dm.tx_limit_enable == 0
        && dm.tx_time == super::state::TX_TIME_DEF
        && dm.tx_retry == super::state::TX_RETRY_DEF
    {
        return Ok(());
    }
    not_ported("TX time limit (rtw89_tx_time_iter)", dm.tx_time)
}

// Linux: _set_rf_trx_para (coex.c:4056), fcxtrx 0 with the v0 tables. The WL calls pass
// RTW89_PHY_0 as a bit map (no effect, see set_wl_tx_power) and the BT ones return because BT
// never sent info.
fn set_rf_trx_para<B: Bus + ?Sized>(bus: &mut B, st: &mut BtcState) -> Rtw89Result<()> {
    let bmode = st.dm.tdd_bind.wl_link_mode;
    let rf_band = st.dm.tdd_bind.rf_band;
    if bmode == wlink::NOLINK {
        return Ok(());
    }
    // shared antenna
    let lv = u32::from((st.dm.wl_btg_rx != 0 && st.bt0.link_cnt_now != 0) || st.dm.bt_only != 0);
    st.dm.trx_para_level = lv;
    let tbl = if st.wl.status.traffic_dir & (1 << RTW89_TFC_UL) != 0 {
        RF_UL
    } else {
        RF_DL
    };
    let para = tbl.get(lv as usize).copied().unwrap_or([0; 4]);
    if st.dm.fddt_train != FDDT_DISABLE {
        return not_ported("FDD train", st.dm.fddt_train);
    }
    set_wl_tx_power(bus, st, para[0].into(), RTW89_PHY_0)?;
    set_wl_rx_gain(bus, st, para[1].into(), RTW89_PHY_0)?;
    if st.bt0.info_updates != 0 && rf_band != 0 {
        return not_ported("BT TX power / LNA constrain", rf_band); // _set_bt_tx_power/_rx_gain
    }
    // wl_stb_chg is 0 while BT is off
    let s = &st.wl.status;
    let stb = u32::from(
        !(st.bt0.enable_now == 0 || st.dm.wl_only != 0 || s.rf_off != 0 || s.lps == BTC_LPS_RF_OFF),
    );
    if stb != st.dm.wl_stb_chg {
        st.dm.wl_stb_chg = stb;
        btc_wl_s1_standby(bus, stb != 0)?;
    }
    Ok(())
}

// Linux: _set_fw_report_map (coex.c:6619)
fn set_fw_report_map<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    hooks: &mut dyn BtcHooks<B>,
) -> Rtw89Result<()> {
    let s = &st.wl.status;
    if s.rf_off == 1 || s.lps != BTC_LPS_OFF {
        return fw_en_rpt(bus, st, hooks, false, 0);
    }
    let bt0 = &st.bt0;
    let mut map = st.fwinfo.rpt_en_map | RPT_MREG;
    if bt0.run_patch_code != 0 && bt0.enable_now != 0 {
        map |= RPT_BT_SCAN_INFO;
    } else {
        map &= !RPT_BT_SCAN_INFO;
    }
    let fw_ver = bt0.ver_fw & 0xffff;
    let ver_unknown = fw_ver == 0 || (fw_ver == BT_ROM_CODE_8852 && bt0.run_patch_code != 0);
    if bt0.enable_now != 0 && ver_unknown {
        map |= RPT_BT_VER_INFO;
    } else {
        map &= !RPT_BT_VER_INFO;
    }
    let afh = RPT_BT_AFH_MAP | RPT_BT_AFH_MAP_LE | RPT_BT_TX_PWR_LVL;
    if bt0.link_cnt_now != 0 {
        map |= afh;
    } else {
        map &= !afh;
    }
    map &= !RPT_BT_DEVICE_INFO; // no A2DP play latency without BT info
    fw_en_rpt(bus, st, hooks, false, map)
}

// Linux: _action_common (coex.c:6679)
pub fn action_common<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    hooks: &mut dyn BtcHooks<B>,
) -> Rtw89Result<()> {
    wl_req_mac(bus, st, st.wl.role.pta_req_band)?;
    update_zb_coex_tbl(bus, st)?;
    set_btg_ctrl(st)?;
    set_wl_preagc_ctrl(bus, st)?;
    set_wl_tx_limit(st)?;
    // _set_phl_bt_slot_req: TDMA off, so bt_req_en false and the length stays 0
    if st.dm.run_reason != rsn::NTFY_WL_RFK && st.wl.status.transacting == 0 {
        st.dm.bt_req_en = false;
    }
    // _set_bt_afh_info: the WL channel goes to BT only while bt0.enable.now
    if st.bt0.enable_now != 0 {
        return not_ported("BT AFH channel info", st.bt0.scbd);
    }
    // _set_bt_rx_agc (coex.c:6540)
    let hi_lna = st.wl.role.link_mode != wlink::NOLINK && st.dm.wl_btg_rx != 0;
    if hi_lna != (st.bt0.hi_lna_rx != 0) {
        write_scbd(bus, st, wscb::BT_HILNA, hi_lna)?;
    }
    set_rf_trx_para(bus, st)?;
    write_scbd(bus, st, wscb::RXSCAN_PRI, st.bt0.scan_rx_low_pri)?; // _set_bt_rx_scan_pri
    set_fw_report_map(bus, st, hooks)?;
    let r = st.dm.run_reason;
    if [rsn::NTFY_INIT, rsn::NTFY_RADIO_STATE, rsn::NTFY_POWEROFF].contains(&r) {
        h2c_cx_role(bus, st, hooks)?;
    }
    // _sned_h2c_w2bscbd (coex.c:6974), BT 1st only (no dual BT), no H2C macro
    if st.wl.scbd_chg != 0 {
        mac_cfg_sb(bus, st.wl.scbd)?;
        st.wl.scbd_chg = 0;
    }
    // fcxosi 0: no OSI info H2C
    st.dm.tdma_instant_excute = 0;
    st.dm.lps_ctrl_change = false;
    st.wl.role.link_mode_chg = 0;
    st.wl.pta_reg_mac_chg = false;
    st.wl.dbcc_chg = 0;
    Ok(())
}
