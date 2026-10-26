//! Bluetooth coexistence of the RTL8852BE, WLAN only (JS js/btc.mjs with btc-run/-hw/-h2c/-c2h/
//! -wlsta.mjs). BitsOS never drives the BT half, so this ports the notifications rtw89 sends to its
//! coex code (coex.c) and exactly the register writes and H2Cs they lead to while the BT side
//! reports nothing: WLAN gets both antenna paths (GNT_WL forced high, GNT_BT forced low), the PTA
//! control path stays with WLAN, TDMA stays off. Every branch that needs a running BT side is
//! refused with [`Rtw89Error::BtcNotPorted`] (with the value that led there), so a wrong
//! assumption shows as an error, not as silent wrong writes.
//!
//! The state is [`BtcState`] (struct rtw89_btc). The coex H2Cs go through [`BtcHooks`]: on CH12
//! with [`H2cLink`] (fw's `h2c_send`), through rfk1's [`MacLink`] or through any [`RfkHooks`]
//! ([`ViaRfk`], for the band switch inside chan's channel switch). The pre-AGC tables are phy's
//! (`phy::bbx::ctrl_nbtg_bt_tx`, the one copy). Linux's warnings and the run's state changes are
//! [`BtcEvent`]s the caller drains (B6). Each function keeps the JS driver's (= Linux's) order of
//! register accesses and names its Linux original.

pub mod c2h;
pub mod common;
pub mod h2c;
pub mod hw;
pub mod role;
pub mod run;
pub mod state;
pub mod wlsta;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_hostile;

use alloc::vec::Vec;
use core::fmt;

use crate::bus::Bus;
use crate::error::{Rtw89Error, Rtw89Result};
use crate::events::EventLog;
use crate::fw::{self, C2h, FwState, H2c};
use crate::pci::PciRings;
use crate::phy::{Band, Phy};
use crate::rfk1::{MacLink, RfkHooks};
use crate::tables::btc::{bscb, rsn, wscb};
use crate::tables::regs::*;

pub use role::{Packet, RoleReq, RoleState};
pub use state::{BtReport, BtcState, BtcVer, VER_0_29_29};
pub use wlsta::WlStaLink;

/// What Linux only prints (rtw89_debug/rtw89_warn) or keeps in its debug counters, and the run's
/// state changes: for the caller's log (B6)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BtcEvent {
    /// `_run_coex` took another action (BTC_ACT_*) than the run before, for `reason` (BTC_RSN_*)
    Action { reason: u32, action: u32 },
    /// rtw89_btc_ntfy_init: "PTA owner warning", the PTA control path (byte `reg` bit 2) was
    /// already with WLAN before the init (`value` the byte read)
    PtaOwnerWarn { reg: u32, value: u32 },
    /// `_send_fw_cmd` refused a coex H2C (`func` of class BTFC_SET, `len` bytes): too long or
    /// empty, WLAN not initialised, or RF off; not sent (Linux counts cnt_h2c_fail)
    H2cRefused { func: u32, len: u32 },
    /// `_fw_set_policy`: the policy is longer than RTW89_BTC_POLICY_MAXLEN; not sent
    PolicyTooLong { len: u32 },
    /// rtw89_mac_cfg_plt_ax: the CMAC is off (`reg` read `value`), the PLT is not written
    PltSkipped { reg: u32, value: u32 },
    /// `_chk_btc_report` stopped the firmware's report walk: report type `typ` (after the
    /// version's renumbering) came with `len` bytes, this version's struct has `want` (0: a type
    /// this version does not know)
    ReportRejected { typ: u32, len: u32, want: u32 },
    /// `_chk_btc_err`: the firmware's TDMA or slot table differed from ours in `count` reports in a
    /// row; the next policy is sent in full
    NoSync { what: &'static str, count: u32 },
    /// A BT scoreboard C2H; bit 1 (BTC_BSCB_ON) set means the BT side says it is on, and the
    /// WLAN-only model no longer holds
    BtScoreboard { bid: u32, scbd: u32 },
    /// rtw89_btc_ntfy_role_info for a port past the role slots: ignored, as Linux does
    RoleIgnored { port: u32 },
}

impl fmt::Display for BtcEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BtcEvent::Action { reason, action } => {
                write!(f, "btc: run for reason {reason} takes action {action}")
            }
            BtcEvent::PtaOwnerWarn { reg, value } => {
                write!(
                    f,
                    "btc: PTA owner warning, {reg:#x} read {value:#04x} before the init"
                )
            }
            BtcEvent::H2cRefused { func, len } => {
                write!(f, "btc: H2C func {func} ({len} bytes) refused, not sent")
            }
            BtcEvent::PolicyTooLong { len } => {
                write!(f, "btc: policy of {len} bytes too long, not sent")
            }
            BtcEvent::PltSkipped { reg, value } => {
                write!(
                    f,
                    "btc: CMAC off ({reg:#x} read {value:#010x}), PLT not written"
                )
            }
            BtcEvent::ReportRejected { typ, len, want } => write!(
                f,
                "btc: firmware report type {typ} with {len} bytes (want {want}), walk stopped"
            ),
            BtcEvent::NoSync { what, count } => {
                write!(
                    f,
                    "btc: firmware {what} differs in {count} reports, policy forced"
                )
            }
            BtcEvent::BtScoreboard { bid, scbd } => {
                let on = if scbd & bscb::ON != 0 { "on" } else { "off" };
                write!(f, "btc: BT {bid} scoreboard {scbd:#010x} (BT says {on})")
            }
            BtcEvent::RoleIgnored { port } => write!(f, "btc: role of port {port} ignored"),
        }
    }
}

/// Where the coex H2Cs go (category OUTSRC, class BTFC_SET; Linux's rtw89_h2c_tx)
pub trait BtcHooks<B: Bus + ?Sized> {
    fn h2c(&mut self, bus: &mut B, h: &H2c, payload: &[u8]) -> Rtw89Result<()>;
}

/// The coex H2Cs on CH12 through fw's `h2c_send`
pub struct H2cLink<'a> {
    pub fw: &'a mut FwState,
    pub rings: &'a mut PciRings,
}

impl<B: Bus + ?Sized> BtcHooks<B> for H2cLink<'_> {
    fn h2c(&mut self, bus: &mut B, h: &H2c, payload: &[u8]) -> Rtw89Result<()> {
        fw::h2c_send(bus, self.rings, self.fw, h, payload)?;
        Ok(())
    }
}

impl<B: Bus + ?Sized> BtcHooks<B> for MacLink<'_> {
    fn h2c(&mut self, bus: &mut B, h: &H2c, payload: &[u8]) -> Rtw89Result<()> {
        MacLink::h2c(self, bus, h, payload)
    }
}

/// The coex H2Cs through an [`RfkHooks`]' own H2C (chan's band switch gets its neighbours so)
pub struct ViaRfk<'a, 'b, B: Bus + ?Sized>(pub &'a mut (dyn RfkHooks<B> + 'b));

impl<B: Bus + ?Sized> BtcHooks<B> for ViaRfk<'_, '_, B> {
    fn h2c(&mut self, bus: &mut B, h: &H2c, payload: &[u8]) -> Rtw89Result<()> {
        self.0.h2c(bus, h, payload)
    }
}

/// enum rtw89_band of a band (as btc.mjs's BAND map)
pub fn band_idx(b: Band) -> u32 {
    match b {
        Band::B2g => RTW89_BAND_2G,
        Band::B5g => RTW89_BAND_5G,
        Band::B6g => 2,
    }
}

pub(crate) fn not_ported<T>(what: &'static str, value: u32) -> Rtw89Result<T> {
    Err(Rtw89Error::BtcNotPorted { what, value })
}

impl BtcState {
    // Linux: rtw89_coex_recognize_ver (coex.c:13320) at firmware load, then rtw89_btc_init
    // (coex.c:8646) at probe with _reset_btc_var (coex.c:1122, BTC_RESET_ALL). Only the
    // 0.29.29 layout is ported; `fw_code` is RTW89_FW_VER_CODE of the loaded firmware.
    pub fn new(fw_code: u32) -> Rtw89Result<BtcState> {
        if !(state::VER_CODE_FIRST..state::VER_CODE_END).contains(&fw_code) {
            return not_ported("firmware coex layout", fw_code);
        }
        let mut st = BtcState {
            ver: VER_0_29_29,
            fwinfo: state::FwInfo::default(),
            update_policy_force: false,
            policy_type: 0,
            ant_type: 0,
            btg_pos: 0,
            wl: state::wl_info(),
            bt0: state::BtInfo::default(),
            bt_ext_func_type: 0,
            ctrl: state::Ctrl {
                trace_step: state::TRACE_STEP_DEF,
                ..state::Ctrl::default()
            },
            dm: state::dm(),
            md: state::module(),
            cnt_power_on: 0,
            bt_reports: EventLog::new(),
            events: EventLog::new(),
        };
        st.wl.status.init_ok = true;
        Ok(st)
    }

    /// What happened since the last call, oldest first
    pub fn take_events(&mut self) -> Vec<BtcEvent> {
        self.events.take()
    }
    pub fn events(&self) -> &[BtcEvent] {
        self.events.as_slice()
    }
    pub fn events_dropped(&self) -> u64 {
        self.events.dropped()
    }
    pub fn set_event_cap(&mut self, cap: usize) {
        self.events.set_cap(cap);
        self.bt_reports.set_cap(cap);
    }
    /// The BT side's C2Hs noted since the last call (scoreboard, BT info)
    pub fn take_bt_reports(&mut self) -> Vec<BtReport> {
        self.bt_reports.take()
    }
    pub fn bt_reports(&self) -> &[BtReport] {
        self.bt_reports.as_slice()
    }
    pub(crate) fn push(&mut self, e: BtcEvent) {
        self.events.push(e);
    }
}

// Linux: rtw89_btc_ntfy_poweron (coex.c:8663): only a counter. The state it needs is
// BtcState::new's (rtw89_coex_recognize_ver at firmware load, rtw89_btc_init at probe).
pub fn power_on(st: &mut BtcState) {
    st.cnt_power_on = st.cnt_power_on.wrapping_add(1);
}

// Linux: _set_init_info (coex.c:8690)
fn set_init_info<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    hooks: &mut dyn BtcHooks<B>,
) -> Rtw89Result<()> {
    st.dm.init_info = state::InitInfo {
        init_mode: st.wl.coex_mode,
        wl_init_ok: u32::from(st.wl.status.init_ok),
        wl_only: st.dm.wl_only,
        bt_only: st.dm.bt_only,
        cx_other: st.bt_ext_func_type,
        wl_guard_ch: 6, // chip->afh_guard_ch
        dbcc_en: 0,
    };
    h2c::h2c_cx_init(bus, st, hooks)?;
    h2c::h2c_cx_ctrl(bus, st, hooks)?;
    h2c::fw_set_slots(bus, st, hooks)?;
    h2c::fw_set_monreg(bus, st, hooks)?;
    // _set_ext_interface: fcxgpiodbg 1 < 7, nothing
    run::set_wl_tx_power(bus, st, RTW89_BTC_WL_DEF_TX_PWR, RTW89_PHY_0)
}

/// The chip facts btc_set_rfe reads (the efuse's rfe_type, hal.cv)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChipFacts {
    pub rfe: u32,
    pub cv: u32,
}

// Linux: rtw89_btc_ntfy_init (coex.c:8723), mode BTC_MODE_NORMAL from rtw89_core_start. 8852B
// para_ver 0: no BTC_FEAT_DUAL_BT, no BTC_FEAT_H2C_MACRO (io_oflld_type NO_SUPPORT).
pub fn init<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    hooks: &mut dyn BtcHooks<B>,
    chip: ChipFacts,
    mode: u32,
) -> Rtw89Result<()> {
    st.wl.coex_mode = mode;
    st.dm.wl_only = u32::from(mode == BTC_MODE_WL);
    st.dm.bt_only = u32::from(mode == BTC_MODE_BT);
    st.wl.status.rf_off = u32::from(mode == BTC_MODE_WLOFF);
    st.ctrl.always_freerun = mode == BTC_MODE_COTX;
    if st.dm.bt_only != 0 || st.wl.status.rf_off != 0 || st.ctrl.always_freerun {
        return not_ported("coex mode", mode);
    }
    if !st.wl.status.init_ok {
        return Ok(());
    }
    st.bt0.enable_now = 1;
    st.bt0.run_patch_code = 1;
    hw::btc_set_rfe(st, chip.rfe, chip.cv);
    if st.ant_type != BTC_ANT_SHARED {
        return not_ported("dedicated antenna (rfe_type even)", chip.rfe);
    }
    hw::btc_init_cfg(bus, st)?;
    run::write_scbd(bus, st, wscb::ACTIVE | wscb::ON | wscb::BTLOG, true)?;
    let (warn, byte) = hw::mac_get_ctrl_path(bus)?;
    st.dm.pta_owner_warn = warn;
    if warn {
        st.push(BtcEvent::PtaOwnerWarn {
            reg: R_AX_SYS_SDIO_CTRL + 3,
            value: byte,
        });
    }
    set_init_info(bus, st, hooks)?;
    run::set_wl_tx_power(bus, st, RTW89_BTC_WL_DEF_TX_PWR, RTW89_PHY_0)?;
    h2c::fw_set_monreg(bus, st, hooks)?;
    h2c::fw_set_slots(bus, st, hooks)?;
    h2c::h2c_cx_init(bus, st, hooks)?;
    h2c::h2c_cx_ctrl(bus, st, hooks)?;
    run::run_coex(bus, st, hooks, rsn::NTFY_INIT)
}

/// The chip facts of phy's efuse parse and hal.cv (refused before any access without the efuse)
pub fn chip_facts(phy: &Phy) -> Rtw89Result<ChipFacts> {
    let ef = phy.efuse.as_ref().ok_or(Rtw89Error::Invalid {
        what: "btc init before the efuse",
        value: 0,
    })?;
    Ok(ChipFacts {
        rfe: ef.rfe_type.into(),
        cv: phy.cv,
    })
}

/// rtw89_btc_ntfy_init(BTC_MODE_NORMAL) where rtw89_core_start calls it (the bring-up's stage phy,
/// `PhyStageHooks::btc_init`): the H2Cs on CH12
pub fn core_start_init<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    rings: &mut PciRings,
    fws: &mut FwState,
    phy: &Phy,
) -> Rtw89Result<()> {
    let chip = chip_facts(phy)?;
    let mut link = H2cLink { fw: fws, rings };
    init(bus, st, &mut link, chip, BTC_MODE_NORMAL)
}

// Linux: rtw89_btc_ntfy_radio_state (coex.c:9345)
fn radio_state<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    hooks: &mut dyn BtcHooks<B>,
    on: bool,
) -> Rtw89Result<()> {
    let s = &mut st.wl.status;
    s.rf_off = u32::from(!on);
    s.lps = BTC_LPS_OFF;
    if !on {
        s.busy = 0;
    }
    if on {
        run::write_scbd(bus, st, wscb::ACTIVE | wscb::ON | wscb::BTLOG, true)?;
        hw::btc_init_cfg(bus, st)?;
    } else {
        run::write_scbd(bus, st, wscb::ALL, false)?;
    }
    st.dm.tdma_instant_excute = 1;
    run::run_coex(bus, st, hooks, rsn::NTFY_RADIO_STATE)?;
    st.wl.status.rf_off_pre = st.wl.status.rf_off;
    st.wl.status.lps_pre = st.wl.status.lps;
    Ok(())
}

// Linux: rtw89_btc_ntfy_radio_state (coex.c:9345) with BTC_RFCTRL_WL_ON, end of rtw89_core_start
pub fn radio_on<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    hooks: &mut dyn BtcHooks<B>,
) -> Rtw89Result<()> {
    radio_state(bus, st, hooks, true)
}

// Linux: rtw89_btc_ntfy_radio_state (coex.c:9345) with BTC_RFCTRL_WL_OFF, start of
// rtw89_core_stop
pub fn radio_off<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    hooks: &mut dyn BtcHooks<B>,
) -> Rtw89Result<()> {
    radio_state(bus, st, hooks, false)
}

// Linux: rtw89_btc_ntfy_poweroff (coex.c:8671), in rtw89_core_stop before the power off
pub fn power_off<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    hooks: &mut dyn BtcHooks<B>,
) -> Rtw89Result<()> {
    let s = &mut st.wl.status;
    s.rf_off = 1;
    s.busy = 0;
    s.lps = BTC_LPS_OFF;
    run::write_scbd(bus, st, wscb::ALL, false)?;
    run::run_coex(bus, st, hooks, rsn::NTFY_POWEROFF)?;
    st.wl.status.rf_off_pre = st.wl.status.rf_off;
    Ok(())
}

// Linux: rtw89_btc_ntfy_scan_start (coex.c:8784) / rtw89_btc_ntfy_scan_finish (coex.c:8814) for
// PHY 0 on `band` (the current channel's); CXDRVINFO_SCAN has no H2C for this version, dbcc is
// off, and firmware 0.29.29 has SCAN_OFFLOAD
pub fn scan<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    hooks: &mut dyn BtcHooks<B>,
    start: bool,
    band: Band,
) -> Rtw89Result<()> {
    let wl = &mut st.wl;
    if start {
        wl.status.scan = 1;
        wl.scan_band[0] = band_idx(band);
        wl.scan_phy_map |= 1;
        st.ctrl.fw_scan = 1;
        return run::run_coex(bus, st, hooks, rsn::NTFY_SCAN_START);
    }
    wl.status.scan = 0;
    wl.scan_phy_map &= !1;
    st.dm.tdma_instant_excute = 1;
    run::run_coex(bus, st, hooks, rsn::NTFY_SCAN_FINISH)
}

// Linux: rtw89_btc_ntfy_switch_band (coex.c:8837), from __rtw89_set_channel when the band changed
// or the channel context was not active yet; `band` only matters with dbcc (off)
pub fn switch_band<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    hooks: &mut dyn BtcHooks<B>,
    _band: Band,
) -> Rtw89Result<()> {
    run::run_coex(bus, st, hooks, rsn::NTFY_SWBAND)
}

// Linux: rtw89_btc_c2h_handle (coex.c:9767), reached from rtw89_fw_c2h_cmd_handle for category
// OUTSRC, classes RTW89_PHY_C2H_CLASS_BTC_MIN..MAX. rtw89_btc_c2h_get_index_by_ver (fwc2hfunc
// 1) keeps RPT, BT_INFO and BT_SCBD as they are; the other events only count or feed debugfs.
// Reports go through btc_fw_event; BT scoreboard and BT info events are only noted (the BT state
// machine is not fed). Returns true if the BT side says it is on.
pub fn c2h(st: &mut BtcState, c: &C2h) -> bool {
    if c.cat != RTW89_C2H_CAT_OUTSRC || c.cls != BTFC_FW_EVENT {
        return false;
    }
    let bid = u32::from(c.func & BT_C2H_FUNC_BT2ND != 0);
    let func = c.func & !BT_C2H_FUNC_BT2ND;
    if func == BTF_EVNT_RPT {
        c2h::fw_event(st, &c.payload);
        return false;
    }
    if func == BTF_EVNT_BT_SCBD
        && let Some(b) = c.payload.get(..4)
    {
        let scbd = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        st.bt_reports.push(BtReport::Scbd { bid, scbd });
        st.push(BtcEvent::BtScoreboard { bid, scbd });
        return scbd & bscb::ON != 0;
    }
    if func == BTF_EVNT_BT_INFO {
        st.bt_reports.push(BtReport::Info {
            bid,
            data: c.payload.clone(),
        });
    }
    false
}

// Linux: rtw89_mac_get_sb (mac.c:6629): the scoreboard word, for a look on the chip
pub fn bt_scoreboard<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<u32> {
    hw::mac_get_sb(bus)
}
