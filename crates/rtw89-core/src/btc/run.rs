//! `_run_coex` (coex.c:8419) with the antenna and policy steps it reaches on the RTL8852B (rfe 1,
//! shared antenna) when no BT driver runs (JS js/btc-run.mjs; `_action_common` is common.rs): the
//! BT scoreboard never reports BT on, so after the init run every run takes `_action_bt_off` (or
//! `_action_wl_off` when WLAN goes down). Branches that need a running BT side are refused.

use crate::bus::Bus;
use crate::error::Rtw89Result;
use crate::tables::btc::{CXTBL, OFF_TBL, S_DEF, T_DEF_OFF, act, bscb, rsn, wlink, wscb};
use crate::tables::regs::*;

use super::h2c::{policy_tlvs, send_fw_cmd};
use super::hw::{mac_cfg_ctrl_path, mac_cfg_gnt, mac_cfg_plt, mac_cfg_sb};
use super::{BtcEvent, BtcHooks, BtcState, not_ported};

pub use super::common::{action_common, set_wl_rx_gain, set_wl_tx_power};

/// enum btc_phymap (coex.c:528)
const BTC_PHY_0: u32 = 1;
const BTC_PHY_ALL: u32 = 3;
/// enum btc_gnt_state (coex.c:902)
const GNT_HW: u32 = 0;
const GNT_SW_LO: u32 = 1;
const GNT_SW_HI: u32 = 2;
/// enum btc_ant_phase (coex.c:543)
pub const ANT_WINIT: u32 = 1;
pub const ANT_WONLY: u32 = 2;
pub const ANT_WOFF: u32 = 3;
/// enum btc_plt (coex.c:558)
const PLT_GNT_WL: u32 = 1 << 3;
/// The TDMA-off policies (coex.c:568, :591): BTC_CXP_OFF << 8 | n
const CXP_OFF: u32 = 0;
pub const CXP_OFF_BT: u32 = 0x0000;
pub const CXP_OFF_EQ0: u32 = 0x0002;
pub const CXP_OFF_WL2: u32 = 0x000c;
/// SLOT_ISO and the slot ids (enum btc_cxst_state) _set_policy_v1 touches
const SLOT_ISO: u16 = 1;
const CXST_OFF: usize = 0;
const CXST_E2G: usize = 0xb;
const CXST_E5G: usize = 0xc;
const CXST_EBT: usize = 0xd;
/// RTW89_BTC_POLICY_MAXLEN (core.h:4391)
const POLICY_MAXLEN: usize = 512;

// Linux: _write_scbd (coex.c:7011). The 8852B has one BT (no BTC_FEAT_DUAL_BT) and no H2C macro,
// so an instant write (CTCODE, RXGAIN) goes to rtw89_mac_cfg_sb and the others wait for
// _action_common.
pub fn write_scbd<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    val: u32,
    state: bool,
) -> Rtw89Result<()> {
    let wl = &mut st.wl;
    let scbd = if state { wl.scbd | val } else { wl.scbd & !val };
    if val & (wscb::CTCODE | wscb::RXGAIN | wscb::RXGAIN_56G) != 0 {
        wl.scbd = scbd;
        mac_cfg_sb(bus, scbd)?;
    } else if val & (wscb::ACTIVE | wscb::ON | wscb::WLRFK) != 0 || scbd != wl.scbd {
        wl.scbd = scbd;
        wl.scbd_chg = 1;
    }
    Ok(())
}

// Linux: _update_bt_scbd (coex.c:7808) for the 1st BT, from the scoreboard the firmware last
// reported (0 until a BT_SCBD event is fed in), with _update_bt_ctrl_lps (coex.c:7779)
fn update_bt_scbd(st: &mut BtcState) -> Rtw89Result<()> {
    let val = st.bt0.scbd_c2h;
    if val == 0xffff_ffff {
        return Ok(());
    }
    let bt = &mut st.bt0;
    let bit = |m: u32| u32::from(val & m != 0);
    bt.enable_now = bit(bscb::ON);
    if bt.enable_now != 0 && bt.enable_last == 0 {
        return not_ported("BT re-enable (BT scoreboard)", val);
    }
    bt.enable_last = bt.enable_now;
    bt.mbx_avl = bit(bscb::ACT);
    bt.whql_test = bit(bscb::WHQL);
    bt.btg_type = if val & bscb::BT_S1 != 0 {
        BTC_BT_BTG
    } else {
        BTC_BT_ALONE
    };
    bt.a2dp = bit(bscb::A2DP_ACT);
    bt.pan = bit(bscb::PAN_ACT);
    bt.hfp = bit(bscb::HFP_ACT);
    bt.lna_constrain = 4 + bit(bscb::BT_LNAB0) + 2 * bit(bscb::BT_LNAB1);
    bt.hi_lna_rx = bit(bscb::BT_HILNA);
    bt.connect = bit(bscb::BT_CONNECT);
    bt.run_patch_code = bit(bscb::PATCH_CODE);
    bt.scbd = val;
    let dm = &mut st.dm;
    let lps_ctrl = !(dm.out_of_band || dm.freerun)
        && (bt.whql_test != 0 || bt.a2dp != 0 || bt.hfp != 0 || bt.pan != 0);
    dm.lps_ctrl_change = dm.lps_ctrl_scbd != lps_ctrl;
    dm.lps_ctrl_scbd = lps_ctrl;
    Ok(())
}

// Linux: _set_coex_binding (coex.c:8288) with _set_bind_info (coex.c:8098). No BT band map is
// ever set (no BT info), so WL and BT never overlap: both binds get rf_band 0, HW band 0 and the
// role's link mode (out of band).
fn set_coex_binding(st: &mut BtcState) {
    let mode = st.wl.role.link_mode;
    let dm = &mut st.dm;
    for bd in [&mut dm.tdd_bind, &mut dm.fdd_bind] {
        bd.rf_band = 0;
        bd.bt_sel = 0;
        bd.bt_link_weight = 0;
        bd.wl_hwb_sel = 1 << RTW89_MAC_0;
        bd.wl_link_mode = mode;
    }
    dm.out_of_band = dm.tdd_bind.rf_band == 0 && dm.fdd_bind.rf_band == 0;
    dm.fdd_en = dm.fdd_bind.rf_band != 0;
    dm.tdd_en = dm.tdd_bind.rf_band != 0;
}

// Linux: _set_gnt (coex.c:3616) -> rtw89_mac_cfg_gnt; the 8852B has no WLAN_ACT mux (para_ver
// 0), so wlact_state changes nothing
fn set_gnt<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    phy_map: u32,
    wl_state: u32,
    bt_state: u32,
) -> Rtw89Result<()> {
    let sw = |state: u32| match state {
        GNT_HW => Some((0, 0)),
        GNT_SW_LO => Some((1, 0)),
        GNT_SW_HI => Some((1, 1)),
        _ => None,
    };
    for (i, g) in st.dm.gnt_set.iter_mut().enumerate() {
        if phy_map & (1 << i) == 0 {
            continue;
        }
        if let Some((en, v)) = sw(wl_state) {
            g.gnt_wl_sw_en = en;
            g.gnt_wl = v;
        }
        if let Some((en, v)) = sw(bt_state) {
            g.gnt_bt_sw_en = en;
            g.gnt_bt = v;
        }
    }
    mac_cfg_gnt(bus, &st.dm.gnt_set)?;
    Ok(())
}

// Linux: _set_ant (coex.c:5603) for the phases a WLAN-only run reaches, then
// rtw89_chip_cfg_ctrl_path and _set_bt_plut (coex.c:5572: fwlrole 2, band 0, dbcc off)
fn set_ant<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    force: bool,
    phy_map: u32,
    typ: u32,
) -> Rtw89Result<()> {
    let path = (phy_map << 8).wrapping_add(typ);
    let reason = st.dm.run_reason;
    let mut force = force
        || st.wl.role.dbcc_chg != 0
        || [rsn::NTFY_POWEROFF, rsn::NTFY_RADIO_STATE, rsn::CMD_SET_COEX].contains(&reason);
    if st.wl.role.link_mode != wlink::DB_MCC && st.dm.wl_btg_rx == 2 {
        force = true;
    }
    if !force && path == st.dm.set_ant_path {
        return Ok(());
    }
    // "return by wl rfk": a calibration runs
    if reason != rsn::NTFY_WL_RFK && st.wl.rfk_info.state != BTC_WRFK_STOP {
        return Ok(());
    }
    st.dm.set_ant_path = path;
    let mut by_wl = true;
    match typ {
        ANT_WINIT => {
            if st.bt0.enable_now != 0 && st.bt0.run_patch_code != 0 {
                set_gnt(bus, st, BTC_PHY_ALL, GNT_SW_LO, GNT_SW_HI)?;
            } else {
                // BT off: WLAN owns both paths
                set_gnt(bus, st, BTC_PHY_ALL, GNT_SW_HI, GNT_SW_LO)?;
            }
        }
        ANT_WONLY => set_gnt(bus, st, BTC_PHY_ALL, GNT_SW_HI, GNT_SW_LO)?,
        ANT_WOFF => {
            by_wl = false; // BTC_CTRL_BY_BT
            set_gnt(bus, st, BTC_PHY_ALL, GNT_SW_LO, GNT_SW_HI)?;
        }
        _ => return not_ported("antenna phase", typ),
    }
    mac_cfg_ctrl_path(bus, by_wl)?;
    if phy_map & BTC_PHY_0 != 0
        && let Some(value) = mac_cfg_plt(bus, PLT_GNT_WL, PLT_GNT_WL)?
    {
        st.push(BtcEvent::PltSkipped {
            reg: R_AX_CMAC_FUNC_EN,
            value,
        });
    }
    Ok(())
}

// Linux: _set_tdma_bind (coex.c:4717)
fn set_tdma_bind<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    tdma_on: bool,
) -> Rtw89Result<()> {
    let bind = if st.dm.tdd_en {
        st.dm.tdd_bind
    } else {
        st.dm.fdd_bind
    };
    if bind.bt_sel & 2 != 0 {
        return not_ported("2nd BT TDMA bind", bind.bt_sel);
    }
    write_scbd(bus, st, wscb::TDMA, tdma_on)?; // bt_sel BTC_BT_1ST
    let dm = &mut st.dm;
    dm.tdma[3] = ((((bind.bt_sel & 0xf) << 4) + (bind.wl_hwb_sel & 0x3))
        | if dm.tdd_en { 4 } else { 0 }
        | if dm.fdd_en { 8 } else { 0 }) as u8;
    Ok(())
}

// Linux: rtw89_btc_set_policy_v1 (coex.c:5097) for the TDMA-off policies
fn set_policy_v1<B: Bus + ?Sized>(bus: &mut B, st: &mut BtcState, typ: u32) -> Rtw89Result<()> {
    if st.wl.status.lps != 0 {
        for sid in [CXST_E2G, CXST_E5G, CXST_EBT] {
            if let (Some(s), Some(d)) = (st.dm.slot.get_mut(sid), S_DEF.get(sid)) {
                *s = *d;
            }
        }
    }
    let idx = (typ & 0xff) as usize;
    let Some(&tbl) = OFF_TBL.get(idx) else {
        return not_ported("coex policy", typ);
    };
    if (typ >> 8) & 0xff != CXP_OFF {
        return not_ported("coex policy", typ);
    }
    let dm = &mut st.dm;
    dm.tdma.copy_from_slice(T_DEF_OFF);
    if let Some(s) = dm.slot.get_mut(CXST_OFF) {
        *s = S_DEF[CXST_OFF];
        s.cxtbl = CXTBL.get(usize::from(tbl)).copied().unwrap_or(0);
        if typ == CXP_OFF_WL2 || typ == CXP_OFF_EQ0 {
            s.cxtype = SLOT_ISO;
        }
    }
    // the no-sync errors come from the firmware's TDMA / slot reports (c2h)
    if dm.tdma_instant_excute != 0 || dm.error_tdma || dm.error_slot || dm.lps_ctrl_change {
        dm.tdma[7] |= 1; // option_ctrl BIT(0)
        st.update_policy_force = true;
    }
    set_tdma_bind(bus, st, false)
}

// Linux: _set_policy (coex.c:4708) = rtw89_btc_set_policy_v1 + _fw_set_policy (coex.c:3210).
// rtw89_set_coex_ctrl_lps only leaves LPS, which BitsOS never enters.
pub fn set_policy<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    hooks: &mut dyn BtcHooks<B>,
    typ: u32,
    action: u32,
) -> Rtw89Result<()> {
    set_policy_v1(bus, st, typ)?;
    if st.dm.run_action != action {
        st.push(BtcEvent::Action {
            reason: st.dm.run_reason,
            action,
        });
    }
    st.dm.run_action = action;
    st.policy_type = typ;
    let policy = policy_tlvs(st);
    if policy.is_empty() {
        return Ok(());
    }
    if policy.len() > POLICY_MAXLEN {
        st.push(BtcEvent::PolicyTooLong {
            len: policy.len() as u32,
        });
        return Ok(());
    }
    if send_fw_cmd(bus, st, hooks, SET_CX_POLICY, &policy)? {
        st.dm.tdma_now = st.dm.tdma;
        st.dm.slot_now.clone_from(&st.dm.slot);
    }
    st.update_policy_force = false;
    Ok(())
}

// Linux: _run_coex (coex.c:8419), resolved for a shared-antenna 8852B whose BT side never
// reports itself on (_check_freerun: shared -> no freerun). _update_run_ctrl_info: fcxctrl 1 < 9,
// no H2C.
pub fn run_coex<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    hooks: &mut dyn BtcHooks<B>,
    reason: u32,
) -> Rtw89Result<()> {
    st.dm.run_reason = reason;
    let s = st.wl.status;
    if !s.init_ok {
        return Ok(());
    }
    if s.rf_off_pre == s.rf_off
        && s.lps_pre == s.lps
        && !st.dm.lps_ctrl_change
        && !st.dm.lps_ctrl_scbd
    {
        if reason == rsn::NTFY_POWEROFF || reason == rsn::NTFY_RADIO_STATE {
            return Ok(());
        }
        if s.rf_off == 1 || s.lps == BTC_LPS_RF_OFF {
            return Ok(());
        }
    }
    if reason == rsn::NTFY_INIT || reason == rsn::NTFY_RADIO_STATE {
        update_bt_scbd(st)?;
    }
    st.dm.freerun = false;
    st.dm.fddt_train = 0;
    st.bt0.scan_rx_low_pri = false;
    set_coex_binding(st);
    st.dm.trx_para_level = 0;
    let all = BTC_PHY_ALL;
    if st.dm.wl_only != 0 {
        // _action_wl_only (coex.c:5715)
        set_ant(bus, st, true, all, ANT_WONLY)?;
        set_policy(bus, st, hooks, CXP_OFF_BT, act::WL_ONLY)?;
    } else if s.rf_off != 0 || st.dm.bt_only != 0 || s.lps != 0 {
        // _action_wl_off (coex.c:5729)
        if s.rf_off != 0 || st.dm.bt_only != 0 {
            set_ant(bus, st, false, all, ANT_WOFF)?;
        } else if s.lps == BTC_LPS_RF_ON || st.bt0.whql_test != 0 {
            return not_ported("PTA antenna in LPS", s.lps);
        }
        let p = if st.dm.out_of_band {
            CXP_OFF_EQ0
        } else {
            CXP_OFF_BT
        };
        set_policy(bus, st, hooks, p, act::WL_OFF)?;
    } else if reason == rsn::NTFY_INIT {
        // _action_wl_init (coex.c:5721)
        set_ant(bus, st, true, all, ANT_WINIT)?;
        set_policy(bus, st, hooks, CXP_OFF_BT, act::WL_INIT)?;
    } else if st.bt0.enable_now == 0 && st.bt_ext_func_type == 0 {
        // _action_bt_off (coex.c:5772)
        set_ant(bus, st, true, all, ANT_WONLY)?;
        set_policy(bus, st, hooks, CXP_OFF_BT, act::BT_OFF)?;
    } else {
        return not_ported("coex run with BT on", st.bt0.scbd);
    }
    action_common(bus, st, hooks)
}
