//! RF calibration of the RTL8852B, part 1 (Linux rtw8852b_rfk.c: RCK, DACK, RX DCK, IQK, the RF
//! channel set, the MCC slot) and the chip's RFK entry points of rtw8852b.c (JS js/rfk1.mjs with
//! rfk1-util/-dack/-iqk/-iqkstep/-chan/-tables.mjs). TSSI and DPK are rfk2's.
//!
//! Everything runs on the host; the firmware only hears of it (coexistence notifications, the
//! RF_GET_MCCCH H2C at the end of `rfk_channel`). The state is [`RfkState`] (JS `d.rfk` without
//! `isTssiMode`, which is [`Phy::tssi_mode`]). The neighbours rfk1 calls (btc's notifications,
//! macinit's scheduler TX stop, fw's H2C, rfk2's TSSI and DPK) come in through [`RfkHooks`];
//! [`MacLink`] does macinit's and fw's part with the real modules. Linux's timeouts and failed
//! calibrations only print in Linux: here they set the state's flags as the JS and become
//! [`RfkEvent`]s for the caller's log (B6).

pub mod chan;
pub mod dack;
pub mod iqk;
pub mod iqkstep;
pub mod util;

#[cfg(any(test, feature = "std"))]
pub mod fake;
#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) mod tests_hostile;

use alloc::vec::Vec;
use core::fmt;

use crate::bus::Bus;
use crate::error::{Rtw89Error, Rtw89Result};
use crate::events::EventLog;
use crate::fw::{self, FwState, H2c};
use crate::macinit::{self, MacState, SchTxSel};
use crate::pci::PciRings;
use crate::phy::{Chan, Phy};
use crate::tables::regs::*;

pub use chan::{mcc_get_ch_info, set_channel_rf};
pub use dack::{dack, rck, rx_dck};
pub use iqk::iqk;
pub use util::{RfkChan, chan_get};

/// RTW89_DACK_MSBK_NR (core.h:6619): the DAC MSB calibration values per path and part
pub const MSBK_NR: usize = RTW89_DACK_MSBK_NR as usize;
/// RTW89_IQK_CHS_NR (core.h:6651): the RFK result slots
const IQK_CHS_NR: usize = RTW89_IQK_CHS_NR as usize;
/// RTW89_RFK_CHS_NR (core.h:6636): the channels struct rtw89_rfk_mcc_info_data keeps
const RFK_CHS_NR: usize = 3;

/// The array index of RF path `path` (the 8852B has two); a bigger one is refused
pub(crate) fn path_idx(path: u32) -> Rtw89Result<usize> {
    if path < util::RF_PATH_NUM_8852B {
        Ok(path as usize)
    } else {
        Err(Rtw89Error::Invalid {
            what: "rfk: RF path past the chip's two",
            value: path,
        })
    }
}

/// One path's RCK result (Linux only prints it)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rck {
    /// the 5-bit resistor code (RR_RCKC_CA)
    pub rck_val: u32,
    pub timeout: bool,
}

/// One `_check_addc` average (Linux only prints them): before and after ADDCK per path
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Addc {
    pub path: u32,
    pub dc_re: i32,
    pub dc_im: i32,
}

/// struct rtw89_dack_info (core.h:6620), the fields the 8852B uses
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Dack {
    pub dack_done: bool,
    pub dack_cnt: u32,
    /// [path][part][index]
    pub msbk_d: [[[u32; MSBK_NR]; 2]; 2],
    pub dadck_d: [[u32; 2]; 2],
    pub addck_d: [[u32; 2]; 2],
    pub biask_d: [[u32; 2]; 2],
    pub addck_timeout: [bool; 2],
    pub dadck_timeout: [bool; 2],
    pub msbk_timeout: [bool; 2],
    /// the `_check_addc` averages of the last DACK (five per run)
    pub addc: Vec<Addc>,
}

/// struct rtw89_iqk_info (core.h:6662), the fields the 8852B uses, plus what Linux only prints
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Iqk {
    pub is_iqk_init: bool,
    pub iqk_fail_cnt: u32,
    /// u8 in Linux: wraps at 256
    pub iqk_times: u32,
    pub version: u32,
    pub syn1to2: u32,
    pub iqk_fft_en: bool,
    pub iqk_sram_en: bool,
    pub iqk_cfir_en: bool,
    pub iqk_xym_en: bool,
    /// [slot][path]
    pub lok_cor_fail: [[bool; 2]; IQK_CHS_NR],
    pub lok_fin_fail: [[bool; 2]; IQK_CHS_NR],
    pub iqk_tx_fail: [[bool; 2]; IQK_CHS_NR],
    pub iqk_rx_fail: [[bool; 2]; IQK_CHS_NR],
    pub iqk_channel: [u32; IQK_CHS_NR],
    pub iqk_band: [u32; 2],
    pub iqk_ch: [u32; 2],
    pub iqk_bw: [u32; 2],
    pub nb_txcfir: [u32; 2],
    pub nb_rxcfir: [u32; 2],
    pub bp_txkresult: [u32; 2],
    pub bp_rxkresult: [u32; 2],
    pub bp_iqkenable: [u32; 2],
    pub is_wb_txiqk: [bool; 2],
    pub is_wb_rxiqk: [bool; 2],
    pub is_nbiqk: bool,
    pub iqk_mcc_ch: [[u32; 2]; IQK_CHS_NR],
    pub iqk_table_idx: [u32; 2],
    pub lok_idac: [[u32; 2]; IQK_CHS_NR],
    pub lok_vbuf: [[u32; 2]; IQK_CHS_NR],
    /// the LOK still failed after its three tries (Linux only prints it)
    pub lok_fail: [bool; 2],
    /// the restore one-shot failed (Linux only prints it)
    pub restore_fail: [bool; 2],
}

/// struct rtw89_rfk_mcc_info_data (core.h:6639): the RFK channel slots
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Mcc {
    pub ch: [u32; RFK_CHS_NR],
    pub band: [u32; RFK_CHS_NR],
    pub bw: [u32; RFK_CHS_NR],
    pub rf18: [u32; RFK_CHS_NR],
    pub table_idx: u8,
}

impl Mcc {
    /// `table_idx` as an index of the IQK result slots (only 0 and 1 exist)
    pub fn slot(&self) -> Rtw89Result<usize> {
        let i = usize::from(self.table_idx);
        if i < IQK_CHS_NR {
            Ok(i)
        } else {
            Err(Rtw89Error::Invalid {
                what: "rfk: mcc table_idx past the IQK slots",
                value: self.table_idx.into(),
            })
        }
    }
}

/// What `_lck_check` tried to get the LO locked (Linux only prints it)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LckStep {
    /// "mmd reset"
    MmdReset,
    /// "re-set rf 0x18"
    ResetRf18,
    /// "syn off/on"
    SynOffOn,
}

/// What Linux only prints (rtw89_debug) or does not report at all in the calibrations: for the
/// caller's log (B6). None of them stops a calibration, as in Linux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RfkEvent {
    /// `_rck`: RR_RCKS bit 3 never set within 30 us (`value` the last read)
    RckTimeout { path: u32, value: u32 },
    /// `_drck`: R_DRCK_RS done never set within 10 ms
    DrckTimeout { value: u32 },
    /// `_addck`: R_ADDCKR0/1 bit 0 never set within 10 ms
    AddckTimeout { path: u32, value: u32 },
    /// `_dack_s0/_s1`: the MSBK OK bits never set within 10 ms
    MsbkTimeout { path: u32 },
    /// `_dack_s0/_s1`: the DADCK OK bits never set within 10 ms
    DadckTimeout { path: u32 },
    /// `_wait_rx_mode`: RF mode stayed 2 (TX) for 5 ms
    RxModeTimeout { path: u32, mode: u32 },
    /// `_iqk_check_cal`: the NCTL engine never wrote 0x55 to 0xbff8 within 8.2 ms (a failed
    /// one-shot)
    NctlTimeout { path: u32, value: u32 },
    /// `_iqk_by_path`: the LOK failed and is tried again with `ibias`
    LokRetry { path: u32, ibias: u32 },
    /// `_iqk_by_path`: the LOK still failed after three tries
    LokFailed { path: u32 },
    /// `_txk_group_sel`: a TX IQK group failed (R_IQKINF says which)
    TxkFailed { path: u32 },
    /// `_rxk_group_sel`: an RX IQK group failed (R_IQKINF says which)
    RxkFailed { path: u32 },
    /// `_iqk_restore`: the restore one-shot failed
    IqkRestoreFailed { path: u32 },
    /// `_set_s0_arfc18`: RR_LPF busy for 1 ms (the LO check is skipped)
    LpfBusyTimeout { value: u32 },
    /// `_lck_check`: the LO was not locked, `step` was tried
    LoLockRemedy { step: LckStep },
}

impl fmt::Display for RfkEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RfkEvent::RckTimeout { path, value } => {
                write!(f, "[RCK] S{path} RCK timeout (RR_RCKS 0x{value:x})")
            }
            RfkEvent::DrckTimeout { value } => write!(f, "[DACK] DRCK timeout (0x{value:x})"),
            RfkEvent::AddckTimeout { path, value } => {
                write!(f, "[DACK] S{path} ADDCK timeout (0x{value:x})")
            }
            RfkEvent::MsbkTimeout { path } => write!(f, "[DACK] S{path} MSBK timeout"),
            RfkEvent::DadckTimeout { path } => write!(f, "[DACK] S{path} DADCK timeout"),
            RfkEvent::RxModeTimeout { path, mode } => {
                write!(f, "[RFK] S{path} still in RF mode {mode} after 5 ms")
            }
            RfkEvent::NctlTimeout { path, value } => {
                write!(f, "[IQK] S{path} one-shot timeout (0xbff8 = 0x{value:x})")
            }
            RfkEvent::LokRetry { path, ibias } => {
                write!(f, "[IQK] S{path} LOK failed, retry with ibias {ibias}")
            }
            RfkEvent::LokFailed { path } => write!(f, "[IQK] S{path} LOK failed"),
            RfkEvent::TxkFailed { path } => write!(f, "[IQK] S{path} TXK failed"),
            RfkEvent::RxkFailed { path } => write!(f, "[IQK] S{path} RXK failed"),
            RfkEvent::IqkRestoreFailed { path } => write!(f, "[IQK] S{path} restore failed"),
            RfkEvent::LpfBusyTimeout { value } => {
                write!(f, "[RFK] LPF busy timeout (0x{value:x})")
            }
            RfkEvent::LoLockRemedy { step } => write!(
                f,
                "[LCK] LO not locked: {}",
                match step {
                    LckStep::MmdReset => "mmd reset",
                    LckStep::ResetRf18 => "re-set rf 0x18",
                    LckStep::SynOffOn => "syn off/on",
                }
            ),
        }
    }
}

/// The RF calibration part of struct rtw89_dev (JS `d.rfk` without `isTssiMode`)
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RfkState {
    /// per path, after the first RCK
    pub rck: [Option<Rck>; 2],
    pub dack: Dack,
    pub iqk: Iqk,
    /// rtwdev->rfk_mcc.data[0]
    pub mcc: Mcc,
    /// what the last `_lck_check` tried (empty: the LO locked at once)
    pub lck: Vec<LckStep>,
    pub(crate) events: EventLog<RfkEvent>,
}

impl RfkState {
    pub fn new() -> RfkState {
        RfkState::default()
    }
    /// What happened since the last call, oldest first
    pub fn take_events(&mut self) -> Vec<RfkEvent> {
        self.events.take()
    }
    pub fn events(&self) -> &[RfkEvent] {
        self.events.as_slice()
    }
    /// Events lost to the log's cap so far (never reset)
    pub fn events_dropped(&self) -> u64 {
        self.events.dropped()
    }
    /// How many undrained events are kept (default [`crate::events::DEFAULT_EVENT_CAP`])
    pub fn set_event_cap(&mut self, cap: usize) {
        self.events.set_cap(cap);
    }
    pub(crate) fn push(&mut self, e: RfkEvent) {
        self.events.push(e);
    }
    fn lck_step(&mut self, step: LckStep) {
        self.lck.push(step);
        self.push(RfkEvent::LoLockRemedy { step });
    }
}

/// The neighbours rfk1 calls, as Linux calls them (JS rfk1 imports them from btc.mjs,
/// macinit.mjs, fw.mjs and rfk2.mjs). [`MacLink`] has macinit's and fw's part.
pub trait RfkHooks<B: Bus + ?Sized> {
    /// rtw89_btc_ntfy_wl_rfk (coex.c:9466): `phy_map` from rtw89_btc_phymap, `typ` BTC_WRFKT_*,
    /// `state` BTC_WRFK_*
    fn btc_wl_rfk(&mut self, bus: &mut B, phy_map: u32, typ: u32, state: u32) -> Rtw89Result<()>;
    /// rtw89_btc_ntfy_conn_rfk (coex.c:13366)
    fn btc_conn_rfk(&mut self, bus: &mut B, start: bool) -> Rtw89Result<()>;
    /// rtw89_btc_ntfy_preserve_bt_time (coex.c:13351)
    fn btc_preserve_bt_time(&mut self, bus: &mut B, ms: u32) -> Rtw89Result<()>;
    /// rtw89_mac_stop_sch_tx(RTW89_SCH_TX_SEL_ALL), MAC 0: returns tx_en
    fn stop_sch_tx(&mut self, bus: &mut B) -> Rtw89Result<u16>;
    /// rtw89_mac_resume_sch_tx, MAC 0
    fn resume_sch_tx(&mut self, bus: &mut B, tx_en: u16) -> Rtw89Result<()>;
    /// rtw89_h2c_tx of an H2C with this header and payload
    fn h2c(&mut self, bus: &mut B, h: &H2c, payload: &[u8]) -> Rtw89Result<()>;
    /// rfk2: rtw8852b_dpk_init
    fn dpk_init(&mut self, bus: &mut B, phy: &mut Phy) -> Rtw89Result<()>;
    /// rfk2: rtw8852b_dpk
    fn dpk(
        &mut self,
        bus: &mut B,
        phy: &mut Phy,
        chan: Option<&Chan>,
        phy_idx: u32,
    ) -> Rtw89Result<()>;
    /// rfk2: rtw8852b_dpk_track
    fn dpk_track(&mut self, bus: &mut B, phy: &mut Phy) -> Rtw89Result<()>;
    /// rfk2: rtw8852b_tssi
    fn tssi(
        &mut self,
        bus: &mut B,
        phy: &mut Phy,
        chan: Option<&Chan>,
        phy_idx: u32,
        hwtx_en: bool,
    ) -> Rtw89Result<()>;
    /// rfk2: rtw8852b_tssi_scan
    fn tssi_scan(
        &mut self,
        bus: &mut B,
        phy: &mut Phy,
        chan: Option<&Chan>,
        phy_idx: u32,
    ) -> Rtw89Result<()>;
    /// rfk2: rtw8852b_wifi_scan_notify
    fn wifi_scan_notify(
        &mut self,
        bus: &mut B,
        phy: &mut Phy,
        start: bool,
        phy_idx: u32,
    ) -> Rtw89Result<()>;
}

/// The MAC side of the neighbours as the driver holds it: macinit's scheduler TX stop/resume
/// and fw's H2C on CH12, with the real modules. An [`RfkHooks`] implementation delegates its
/// `stop_sch_tx`, `resume_sch_tx` and `h2c` here.
pub struct MacLink<'a> {
    pub mac: &'a mut MacState,
    pub fw: &'a mut FwState,
    /// CH12 for the H2C (`None`: an H2C is refused)
    pub rings: Option<&'a mut PciRings>,
}

impl MacLink<'_> {
    // Linux: rtw89_mac_stop_sch_tx(rtwdev, 0, RTW89_SCH_TX_SEL_ALL) (mac.c:3403)
    pub fn stop_sch_tx<B: Bus + ?Sized>(&mut self, bus: &mut B) -> Rtw89Result<u16> {
        macinit::stop_sch_tx(bus, self.mac, self.fw, SchTxSel::All)
    }
    // Linux: rtw89_mac_resume_sch_tx(rtwdev, 0, tx_en) (mac.c:3485)
    pub fn resume_sch_tx<B: Bus + ?Sized>(&mut self, bus: &mut B, tx_en: u16) -> Rtw89Result<()> {
        macinit::resume_sch_tx(bus, self.mac, self.fw, tx_en)
    }
    // Linux: rtw89_h2c_tx (core.c:1373)
    pub fn h2c<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        h: &H2c,
        payload: &[u8],
    ) -> Rtw89Result<()> {
        let rings = self.rings.as_deref_mut().ok_or(Rtw89Error::Invalid {
            what: "rfk H2C without the PCIe rings",
            value: h.func,
        })?;
        fw::h2c_send(bus, rings, self.fw, h, payload)?;
        Ok(())
    }
}

// Linux: rtw8852b_rfk_init (rtw8852b.c:697). `chan`: the channel context's rtw89_chan (None:
// still zero, as at the bring-up)
pub fn rfk_init<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &mut Phy,
    st: &mut RfkState,
    chan: Option<&Chan>,
    hooks: &mut dyn RfkHooks<B>,
) -> Rtw89Result<()> {
    phy.tssi_mode = [false; 2];
    st.mcc = Mcc::default(); // memset(rfk_mcc); data[1] is unused on 8852B
    hooks.dpk_init(bus, phy)?;
    rck(bus, st)?;
    dack(bus, st, chan, hooks)?;
    rx_dck(bus, phy, st, chan, hooks, RTW89_PHY_0)
}

// Linux: rtw89_fw_h2c_rf_ntfy_mcc (fw.c:8123). 8852B firmware lacks RFK_NTFY_MCC_V0 (fw.c:895
// lists only the 8852C), so the layout is struct rtw89_fw_h2c_rf_get_mccch (fw.h:4911): five
// le32, no acks asked.
pub fn h2c_rf_ntfy_mcc<B: Bus + ?Sized>(
    bus: &mut B,
    st: &RfkState,
    hooks: &mut dyn RfkHooks<B>,
) -> Rtw89Result<()> {
    let m = &st.mcc;
    let cur = *m
        .ch
        .get(usize::from(m.table_idx))
        .ok_or(Rtw89Error::Invalid {
            what: "rfk: mcc table_idx past its channels",
            value: m.table_idx.into(),
        })?;
    let mut p = [0u8; 20];
    for (i, v) in [m.ch[0], m.ch[0], m.ch[1], m.ch[1], cur]
        .into_iter()
        .enumerate()
    {
        p[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes()); // ch_0_0, ch_0_1, ch_1_0, ch_1_1, current_channel
    }
    let h = H2c {
        cat: H2C_CAT_OUTSRC,
        cls: H2C_CL_OUTSRC_RF_FW_NOTIFY,
        func: H2C_FUNC_OUTSRC_RF_GET_MCCCH,
        rack: false,
        dack: false,
    };
    hooks.h2c(bus, &h, &p)
}

// Linux: rtw8852b_rfk_channel (rtw8852b.c:711); `chan` is the new channel (chan's), `phy_idx`
// the vif link's (always 0 on BitsOS)
pub fn rfk_channel<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &mut Phy,
    st: &mut RfkState,
    chan: Option<&Chan>,
    hooks: &mut dyn RfkHooks<B>,
    phy_idx: u32,
) -> Rtw89Result<()> {
    mcc_get_ch_info(st, chan, phy_idx)?;
    hooks.btc_conn_rfk(bus, true)?;
    rx_dck(bus, phy, st, chan, hooks, phy_idx)?;
    iqk(bus, st, chan, hooks, phy_idx)?;
    hooks.btc_preserve_bt_time(bus, 30)?;
    hooks.tssi(bus, phy, chan, phy_idx, true)?;
    hooks.btc_preserve_bt_time(bus, 30)?;
    hooks.dpk(bus, phy, chan, phy_idx)?;
    hooks.btc_conn_rfk(bus, false)?;
    h2c_rf_ntfy_mcc(bus, st, hooks)?;
    bus.flush()?;
    Ok(())
}

// Linux: rtw8852b_rfk_band_changed (rtw8852b.c:731), PHY 0
pub fn rfk_band_changed<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &mut Phy,
    chan: Option<&Chan>,
    hooks: &mut dyn RfkHooks<B>,
) -> Rtw89Result<()> {
    hooks.tssi_scan(bus, phy, chan, RTW89_PHY_0)
}

// Linux: rtw8852b_rfk_scan (rtw8852b.c:738)
pub fn rfk_scan<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &mut Phy,
    start: bool,
    hooks: &mut dyn RfkHooks<B>,
    phy_idx: u32,
) -> Rtw89Result<()> {
    hooks.wifi_scan_notify(bus, phy, start, phy_idx)
}

// Linux: rtw8852b_rfk_track (rtw8852b.c:746); Linux calls it from the 2-second track work
// (rtw89_core_rfk_track, core.c:5345)
pub fn rfk_track<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &mut Phy,
    hooks: &mut dyn RfkHooks<B>,
) -> Rtw89Result<()> {
    hooks.dpk_track(bus, phy)
}
