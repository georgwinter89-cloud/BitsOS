//! struct rtw89_btc as far as the WLAN-only coexistence reads or writes it (JS `d.btc`, made by
//! btc.mjs `resetBtcVar`), with the field names of Linux (core.h struct rtw89_btc_*).

use alloc::vec::Vec;

use crate::events::EventLog;
use crate::tables::BtcSlot;
use crate::tables::btc::{S_DEF, T_DEF_OFF, act, rsn};
use crate::tables::regs::*;

use super::BtcEvent;

/// RTW89_BE_BTC_WL_MAX_ROLE_NUMBER, the role slots of struct rtw89_btc_wl_role_info (the
/// firmware's `max_role_num` is the same 6 for this layout)
pub const MAX_ROLE: usize = 6;
/// Firmware coex version code of 0.29.29.0 (coex.c:289) and the first code past the layout
pub const VER_CODE_FIRST: u32 = 0x001d_1d00;
pub const VER_CODE_END: u32 = 0x001d_7a00;

/// The rtw89_btc_ver_defs entry for RTL8852B firmware 0.29.29.x to 0.29.121.x (coex.c:289)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BtcVer {
    pub code: u32,
    pub fcxbtcrpt: u8,
    pub fcxtdma: u8,
    pub fcxslots: u8,
    pub fcxcysta: u8,
    pub fcxstep: u8,
    pub fcxnullsta: u8,
    pub fcxmreg: u8,
    pub fcxgpiodbg: u8,
    pub fcxbtver: u8,
    pub fcxbtscan: u8,
    pub fcxbtafh: u8,
    pub fcxbtdevinfo: u8,
    pub fwlrole: u8,
    pub frptmap: u8,
    pub fcxctrl: u8,
    pub fcxinit: u8,
    pub fwevntrptl: u8,
    pub fwc2hfunc: u8,
    pub drvinfo_ver: u8,
    pub info_buf: u32,
    pub max_role_num: u8,
    pub fcxosi: u8,
    pub fcxmlo: u8,
    pub bt_desired: u8,
    pub fcxtrx: u8,
}

pub const VER_0_29_29: BtcVer = BtcVer {
    code: VER_CODE_FIRST,
    fcxbtcrpt: 105,
    fcxtdma: 3,
    fcxslots: 1,
    fcxcysta: 5,
    fcxstep: 3,
    fcxnullsta: 2,
    fcxmreg: 2,
    fcxgpiodbg: 1,
    fcxbtver: 1,
    fcxbtscan: 2,
    fcxbtafh: 2,
    fcxbtdevinfo: 1,
    fwlrole: 2,
    frptmap: 3,
    fcxctrl: 1,
    fcxinit: 0,
    fwevntrptl: 0,
    fwc2hfunc: 1,
    drvinfo_ver: 0,
    info_buf: 1800,
    max_role_num: MAX_ROLE as u8,
    fcxosi: 0,
    fcxmlo: 0,
    bt_desired: 7,
    fcxtrx: 0,
};

/// struct rtw89_btc_wl_smap
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WlStatus {
    pub init_ok: bool,
    pub rf_off: u32,
    pub rf_off_pre: u32,
    /// enum btc_lps_state
    pub lps: u32,
    pub lps_pre: u32,
    pub busy: u32,
    pub scan: u32,
    pub transacting: u32,
    pub four_way: u32,
    pub dhcp: u32,
    pub handshake: u32,
    pub roaming: u32,
    /// bit map of enum rtw89_tfc_dir
    pub traffic_dir: u32,
}

/// struct rtw89_btc_wl_rfk_info
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RfkInfo {
    /// BTC_WRFK_* (0 = STOP)
    pub state: u32,
    /// BTC_WRFKT_*
    pub typ: u32,
    pub path_map: u32,
    pub phy_map: u32,
    pub band: u32,
    pub con_rfk: bool,
}

/// struct rtw89_btc_wl_role_info's rlink entry (struct rtw89_btc_wl_active_role)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rlink {
    pub connected: u32,
    pub pid: u32,
    pub phy: u32,
    pub noa: u32,
    pub rf_band: u32,
    pub active: bool,
    pub bw: u32,
    pub role: u32,
    pub ch: u32,
    pub noa_dur: u32,
    pub client_cnt: u32,
    pub mode: u32,
    pub mac_id: u32,
    pub client_ps: u32,
}

/// The vif's traffic statistics as the track work hands them over (struct rtw89_traffic_stats)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Traffic {
    /// enum rtw89_tfc_lv
    pub tx_tfc_lv: u32,
    pub rx_tfc_lv: u32,
    pub tx_throughput: u32,
    pub rx_throughput: u32,
    /// the "last" rates kept in the link's copy
    pub tx_rate: u32,
    pub rx_rate: u32,
}

/// struct rtw89_btc_wl_link_info of one port: what the last role notification said, plus what
/// the track work's station iteration keeps (a role notification starts it afresh, as the JS)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkInfo {
    pub mode: u32,
    pub role: u32,
    pub phy: u32,
    pub pid: u32,
    pub active: bool,
    /// enum rtw89_band
    pub band: u32,
    pub ch: u32,
    /// enum rtw89_bandwidth
    pub bw: u32,
    pub mac_id: u32,
    /// MLME_NO_LINK / _LINKING / _LINKED
    pub connected: u32,
    /// the station iteration's copy of the vif's traffic statistics
    pub traffic: Option<Traffic>,
    /// the link's RSSI as the last iteration of a connected link read it
    pub rssi: Option<u32>,
    /// enum btc_rssi_st per threshold of rtw89_btc_8852b_wl_rssi_thres
    pub rssi_state: Option<[u32; 4]>,
    pub busy: u32,
    pub dir: u32,
    pub rx_rate_drop_cnt: u32,
}

/// struct rtw89_btc_wl_role_info (non-MLO part)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleInfo {
    pub rlink: [Rlink; MAX_ROLE],
    pub link_mode: u32,
    pub link_mode_hb1: u32,
    pub link_mode_chg: u32,
    pub p2p_exist: u32,
    pub pta_req_band: u32,
    pub dbcc_en: u32,
    pub dbcc_2g_phy: u32,
    pub role_map: u32,
    pub mrole_type: u32,
    pub mrole_noa_duration: u32,
    pub connect_cnt: u32,
    pub dbcc_chg: u32,
}

/// struct rtw89_btc_wl_info
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WlInfo {
    pub coex_mode: u32,
    /// the WL->BT scoreboard word (enum btc_w2b_scoreboard bits)
    pub scbd: u32,
    pub scbd_chg: u32,
    pub pta_req_mac: u32,
    pub pta_reg_mac_chg: bool,
    pub link_mode_chg: u32,
    pub bg_mode: u32,
    /// the WL coex and firmware versions of the firmware's CTRL report
    pub ver_fw_coex: u32,
    pub ver_fw: u32,
    pub is_5g_hi_ch: u32,
    pub go_client_exist: u32,
    pub noa_exist: u32,
    pub dbcc_chg: u32,
    pub rf_band_map: [u32; 2],
    pub scan_band: [u32; 2],
    pub scan_phy_map: u32,
    pub status: WlStatus,
    pub rfk_info: RfkInfo,
    pub rlink_info: [Option<LinkInfo>; MAX_ROLE],
    pub role: RoleInfo,
    pub rssi_level: u32,
}

/// struct rtw89_btc_bt_info of the first BT, as far as its scoreboard fills it
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BtInfo {
    pub enable_now: u32,
    pub enable_last: u32,
    pub run_patch_code: u32,
    /// the BT->WL scoreboard the firmware last reported (0 until a BT_SCBD event is fed in)
    pub scbd_c2h: u32,
    pub scbd: u32,
    pub mbx_avl: u32,
    pub whql_test: u32,
    pub btg_type: u32,
    pub a2dp: u32,
    pub pan: u32,
    pub hfp: u32,
    pub lna_constrain: u32,
    pub hi_lna_rx: u32,
    pub connect: u32,
    pub link_cnt_now: u32,
    pub info_updates: u32,
    pub ver_fw: u32,
    pub scan_rx_low_pri: bool,
}

/// struct rtw89_btc_ctrl
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Ctrl {
    pub manual: u32,
    pub igno_bt: u32,
    pub always_freerun: bool,
    pub trace_step: u32,
    pub fw_scan: u32,
}

/// struct rtw89_btc_gnt of one path (S0, S1)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Gnt {
    pub gnt_bt: u32,
    pub gnt_bt_sw_en: u32,
    pub gnt_wl: u32,
    pub gnt_wl_sw_en: u32,
}

/// struct rtw89_btc_bind_info (TDD or FDD)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Bind {
    pub rf_band: u32,
    pub bt_sel: u32,
    pub bt_link_weight: u32,
    pub wl_hwb_sel: u32,
    pub wl_link_mode: u32,
}

/// struct rtw89_btc_init_info as _set_init_info fills it
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InitInfo {
    pub init_mode: u32,
    pub wl_init_ok: u32,
    pub wl_only: u32,
    pub bt_only: u32,
    pub cx_other: u32,
    pub wl_guard_ch: u32,
    pub dbcc_en: u32,
}

/// What the station iteration hands to the firmware log (struct rtw89_traffic_stats in dm)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TrxInfo {
    pub tx_lvl: u32,
    pub rx_lvl: u32,
    pub tx_rate: u32,
    pub rx_rate: u32,
    pub tx_tp: u32,
    pub rx_tp: u32,
    pub wl_rssi: u32,
}

/// struct rtw89_btc_dm
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dm {
    pub wl_only: u32,
    pub bt_only: u32,
    /// enum btc_reason_and_action
    pub run_reason: u32,
    pub run_action: u32,
    /// struct rtw89_btc_fbtc_tdma as its 8 bytes
    pub tdma: [u8; 8],
    pub tdma_now: [u8; 8],
    /// one entry per slot of s_def (CXST_MAX)
    pub slot: Vec<BtcSlot>,
    pub slot_now: Vec<BtcSlot>,
    pub tdma_instant_excute: u32,
    pub lps_ctrl_scbd: bool,
    pub lps_ctrl_change: bool,
    pub set_ant_path: u32,
    pub gnt_set: [Gnt; 2],
    pub wl_pre_agc: u32,
    pub wl_pre_agc_rb: u32,
    pub wl_btg_rx: u32,
    pub wl_btg_rx_rb: u32,
    pub wl_stb_chg: u32,
    pub wl_lna2: u32,
    pub wl_fw_cx_offload: u32,
    pub error_tdma: bool,
    pub error_slot: bool,
    pub cnt_tdma_nonsync: u32,
    pub cnt_slot_nonsync: u32,
    pub tx_limit_enable: u32,
    pub tx_time: u32,
    pub tx_retry: u32,
    pub trx_para_level: u32,
    pub wl_tx_power: [u32; 2],
    pub wl_rx_gain: [u32; 2],
    pub out_of_band: bool,
    pub tdd_en: bool,
    pub fdd_en: bool,
    pub tdd_bind: Bind,
    pub fdd_bind: Bind,
    pub freerun: bool,
    pub fddt_train: u32,
    pub bt_req_en: bool,
    pub bt_req_len: [u32; 2],
    pub init_info: InitInfo,
    pub pta_owner_warn: bool,
    pub leak_ap: u32,
    pub trx_info: TrxInfo,
    pub cnt_notify_wl_sta: u32,
    pub cnt_dm_wl_sta_last: u32,
}

/// struct rtw89_btc_ant_info
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AntInfo {
    pub typ: u32,
    pub num: u32,
    pub isolation: u32,
    pub diversity: u32,
    pub single_pos: u32,
    pub btg_pos: u32,
    pub stream_cnt: u32,
}

/// struct rtw89_btc_module (btc->mdinfo)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Module {
    pub rfe_type: u32,
    pub kt_ver: u32,
    pub bt_solo: u32,
    pub bt0_sw_type: u32,
    pub bt0_pos: u32,
    pub wa_type: u32,
    pub kt_ver_adie: u32,
    pub ant: AntInfo,
}

/// struct rtw89_btc_fw_info: the report map in force and the H2C counters
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FwInfo {
    pub rpt_en_map: u32,
    pub cnt_h2c: u32,
    pub cnt_h2c_fail: u32,
}

/// What the BT side said through a C2H (only noted: the BT state machine is not fed)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BtReport {
    /// BTF_EVNT_BT_SCBD: the BT->WL scoreboard
    Scbd { bid: u32, scbd: u32 },
    /// BTF_EVNT_BT_INFO: the raw BT info bytes
    Info { bid: u32, data: Vec<u8> },
}

/// struct rtw89_btc of the 8852B, WLAN only (JS `d.btc`)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtcState {
    pub ver: BtcVer,
    pub fwinfo: FwInfo,
    pub update_policy_force: bool,
    pub policy_type: u32,
    /// btc->ant_type and btc->btg_pos (copies of the module's, set by btc_set_rfe)
    pub ant_type: u32,
    pub btg_pos: u32,
    pub wl: WlInfo,
    pub bt0: BtInfo,
    /// cx.bt_ext's func_type
    pub bt_ext_func_type: u32,
    pub ctrl: Ctrl,
    pub dm: Dm,
    pub md: Module,
    /// notifications seen (dm.cnt_notify of POWER_ON; the others are not kept)
    pub cnt_power_on: u32,
    pub(super) bt_reports: EventLog<BtReport>,
    pub(super) events: EventLog<BtcEvent>,
}

fn role_info() -> RoleInfo {
    RoleInfo {
        rlink: [Rlink::default(); MAX_ROLE],
        link_mode: 0,
        link_mode_hb1: 0,
        link_mode_chg: 0,
        p2p_exist: 0,
        pta_req_band: 0,
        dbcc_en: 0,
        dbcc_2g_phy: 0,
        role_map: 0,
        mrole_type: 0,
        mrole_noa_duration: 0,
        connect_cnt: 0,
        dbcc_chg: 0,
    }
}

pub(super) fn wl_info() -> WlInfo {
    WlInfo {
        coex_mode: 0,
        scbd: 0,
        scbd_chg: 0,
        pta_req_mac: 0,
        pta_reg_mac_chg: false,
        link_mode_chg: 0,
        bg_mode: 0,
        ver_fw_coex: 0,
        ver_fw: 0,
        is_5g_hi_ch: 0,
        go_client_exist: 0,
        noa_exist: 0,
        dbcc_chg: 0,
        rf_band_map: [0; 2],
        scan_band: [0; 2],
        scan_phy_map: 0,
        status: WlStatus::default(),
        rfk_info: RfkInfo::default(),
        rlink_info: Default::default(),
        role: role_info(),
        rssi_level: 0,
    }
}

/// BTC_MAX_TX_TIME_DEF / BTC_MAX_TX_RETRY_DEF (coex.c:927, 933)
pub const TX_TIME_DEF: u32 = 5280;
pub const TX_RETRY_DEF: u32 = 31;
/// FCXDEF_STEP: the trace steps of the STEP report
pub const TRACE_STEP_DEF: u32 = 50;

pub(super) fn dm() -> Dm {
    let mut t = [0u8; 8];
    t.copy_from_slice(T_DEF_OFF);
    Dm {
        wl_only: 0,
        bt_only: 0,
        run_reason: rsn::NONE,
        run_action: act::NONE,
        tdma: t,
        tdma_now: t,
        slot: S_DEF.to_vec(),
        slot_now: S_DEF.to_vec(),
        tdma_instant_excute: 0,
        lps_ctrl_scbd: false,
        lps_ctrl_change: false,
        set_ant_path: 0,
        gnt_set: [Gnt::default(); 2],
        wl_pre_agc: 0,
        wl_pre_agc_rb: BTC_PREAGC_NOTFOUND,
        wl_btg_rx: 0,
        wl_btg_rx_rb: BTC_BTGCTRL_BB_GNT_NOTFOUND,
        wl_stb_chg: 0,
        wl_lna2: 0,
        wl_fw_cx_offload: 0,
        error_tdma: false,
        error_slot: false,
        cnt_tdma_nonsync: 0,
        cnt_slot_nonsync: 0,
        tx_limit_enable: 0,
        tx_time: TX_TIME_DEF,
        tx_retry: TX_RETRY_DEF,
        trx_para_level: 0,
        wl_tx_power: [0; 2],
        wl_rx_gain: [0; 2],
        out_of_band: false,
        tdd_en: false,
        fdd_en: false,
        tdd_bind: Bind::default(),
        fdd_bind: Bind::default(),
        freerun: false,
        fddt_train: 0,
        bt_req_en: false,
        bt_req_len: [0; 2],
        init_info: InitInfo::default(),
        pta_owner_warn: false,
        leak_ap: 0,
        trx_info: TrxInfo::default(),
        cnt_notify_wl_sta: 0,
        cnt_dm_wl_sta_last: 0,
    }
}

pub(super) fn module() -> Module {
    Module {
        ant: AntInfo {
            isolation: RTW89_BTC_DEFAULT_ANISO,
            ..AntInfo::default()
        },
        ..Module::default()
    }
}
