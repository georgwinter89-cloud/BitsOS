//! RF calibration of the RTL8852B, part 2: TSSI (closed-loop TX power) and DPK (digital
//! predistortion) of Linux rtw8852b_rfk.c, with the TX power tracking table of the firmware
//! file's TXPWR_TRK element (JS js/rfk2.mjs with rfk2-common/-dpk/-dpkflow/-tssi/-alimk/-trk/
//! -tables/-trkdata.mjs). rfk1 calls the six entries here through its [`RfkHooks`]:
//! `rfk_init` -> [`dpk_init`], `rfk_channel` -> [`tssi`] and [`dpk`], `rfk_band_changed` ->
//! [`tssi_scan`], `rfk_scan` -> [`wifi_scan_notify`], `rfk_track` -> [`dpk_track`];
//! [`Rfk2Link`] wires them in.
//!
//! The state is [`Rfk2State`] (JS `d.rfk.dpk`, `d.rfk.tssi`, `d.rfk.txpwrTrk`,
//! `d.rfk.pollTimeouts`); `is_tssi_mode` is [`Phy::tssi_mode`], the efuse's TSSI and gain values
//! are [`Phy::efuse`]. The neighbours (btc's RFK notification, macinit's scheduler TX stop and
//! resume) come in through [`Rfk2Hooks`]. Linux only prints the timeouts and failed
//! calibrations: here they become [`Rfk2Event`]s for the caller's log (B6), and the state keeps
//! what the JS keeps.

pub mod alimk;
pub mod dpk;
pub mod dpkflow;
pub mod trk;
pub mod tssi;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_hostile;
#[cfg(test)]
mod tests_undo;

use alloc::vec::Vec;
use core::fmt;

use crate::bus::{Bus, BusError, prep};
use crate::error::{Rtw89Error, Rtw89Result};
use crate::events::EventLog;
use crate::fw::H2c;
use crate::phy::efuse::Efuse;
use crate::phy::io::{BB, phy_mask32};
use crate::phy::{Chan, Phy};
use crate::rfk1::util::{self, RF_PATH_NUM_8852B, RfkChan, btc_phymap, chan_get, kpath};
use crate::rfk1::{RfkEvent, RfkHooks};
use crate::tables::regs::*;

pub use trk::TxpwrTrk;

/// RTW89_DPK_BKUP_NUM (core.h:6698): the DPK result slots per path
pub const DPK_BKUP_NUM: usize = 2;
/// TSSI_ALIMK_MAX (core.h:6921): the alignment bands (2 GHz, 5 GHz low/mid/high)
pub const ALIMK_MAX: usize = 4;
/// TSSI_MAX_CH_NUM (core.h:6934): the channels whose alignment is kept (`_tssi_ch_to_idx`)
pub const MAX_CH_NUM: usize = TSSI_MAX_CH_NUM as usize;
/// The alignment words Linux keeps per band and channel (TSSI_ALIMK_VALUE_NUM is 8, the 8852B
/// fills the first 4)
pub const ALIMK_VALUES: usize = 4;
/// RTW8852B_TSSI_PATH_NR / RTW8852B_DPK_RF_PATH / RF_PATH_NUM_8852B: the chip's two paths
const PATHS: usize = RF_PATH_NUM_8852B as usize;

/// struct rtw89_dpk_bkup_para (core.h:6699): one path's DPK result in one slot
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DpkBp {
    /// enum rtw89_band
    pub band: u32,
    /// enum rtw89_bandwidth
    pub bw: u32,
    pub ch: u32,
    pub path_ok: bool,
    pub txagc_dpk: u32,
    pub ther_dpk: u32,
    pub gs: u32,
    pub pwsf: u32,
}

/// struct rtw89_dpk_info (core.h:6711), the fields the 8852B uses
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Dpk {
    pub is_dpk_enable: bool,
    pub is_dpk_reload_en: bool,
    /// per PHY: 0x7f (backoff moved to the BB) or 0x5b
    pub dpk_gs: [u32; 2],
    /// the slot (0 or 1) per path
    pub cur_idx: [u32; PATHS],
    /// [path][slot]
    pub dc_i: [[u32; DPK_BKUP_NUM]; PATHS],
    pub dc_q: [[u32; DPK_BKUP_NUM]; PATHS],
    pub corr_val: [[u32; DPK_BKUP_NUM]; PATHS],
    pub corr_idx: [[u32; DPK_BKUP_NUM]; PATHS],
    pub bp: [[DpkBp; DPK_BKUP_NUM]; PATHS],
}

impl Dpk {
    /// Path `path`'s result in slot `kidx`; a path or slot past the chip's two is refused
    pub(crate) fn at(&mut self, path: u32, kidx: usize) -> Rtw89Result<&mut DpkBp> {
        let p = crate::rfk1::path_idx(path)?;
        self.bp[p].get_mut(kidx).ok_or(Rtw89Error::Invalid {
            what: "rfk2: dpk slot past the two",
            value: kidx as u32,
        })
    }
    /// `cur_idx[path]` as a slot index (only 0 and 1 exist)
    fn slot(&self, path: u32) -> Rtw89Result<usize> {
        let p = crate::rfk1::path_idx(path)?;
        let k = self.cur_idx[p] as usize;
        if k < DPK_BKUP_NUM {
            Ok(k)
        } else {
            Err(Rtw89Error::Invalid {
                what: "rfk2: dpk cur_idx past the slots",
                value: self.cur_idx[p],
            })
        }
    }
}

/// The run-time part of struct rtw89_tssi_info (core.h:6937): the alignment per band and per
/// channel, paths A and B
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tssi {
    /// [path][TSSI_ALIMK_*]
    pub alignment_done: [[bool; ALIMK_MAX]; PATHS],
    pub alignment_value: [[[u32; ALIMK_VALUES]; ALIMK_MAX]; PATHS],
    /// [path][_tssi_ch_to_idx]
    pub check_backup_aligmk: [[bool; MAX_CH_NUM]; PATHS],
    pub alignment_backup_by_ch: [[[u32; ALIMK_VALUES]; MAX_CH_NUM]; PATHS],
}

impl Default for Tssi {
    fn default() -> Tssi {
        Tssi {
            alignment_done: [[false; ALIMK_MAX]; PATHS],
            alignment_value: [[[0; ALIMK_VALUES]; ALIMK_MAX]; PATHS],
            check_backup_aligmk: [[false; MAX_CH_NUM]; PATHS],
            alignment_backup_by_ch: [[[0; ALIMK_VALUES]; MAX_CH_NUM]; PATHS],
        }
    }
}

/// rtwdev->fem (struct rtw89_fem_info, core.h:6726): an external PA skips DPK. Linux never fills it for the
/// 8852B, so it stays false unless a caller knows better.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Fem {
    pub epa_2g: bool,
    pub epa_5g: bool,
}

/// What Linux only prints (rtw89_debug/rtw89_warn) or does not report at all in TSSI and DPK:
/// for the caller's log (B6). None of them stops a calibration, as in Linux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rfk2Event {
    /// `_dpk_one_shot`: the KIP never answered (`addr` the BB register polled, `last` its value
    /// after the wait); the DPD coefficients of this run are not to be trusted
    OneShotTimeout { path: u32, addr: u32, last: u32 },
    /// `_wait_rx_mode`: RF mode stayed 2 (TX) for 5 ms
    RxModeTimeout { path: u32, mode: u32 },
    /// `_dpk_main`: the path's DPK failed (sync report out of range), DPD off for it
    DpkFailed {
        path: u32,
        kidx: u32,
        corr_val: u32,
        dc_i: u32,
        dc_q: u32,
    },
    /// `_dpk_bypass_check`: an external PA, DPK skipped
    DpkSkipped { band: u32 },
    /// `_tssi_get_cw_report`: no TSSI report after 100 tries; the path keeps its default
    /// alignment
    TssiNoReport { path: u32, power: u32 },
    /// The firmware file has no TXPWR_TRK element: Linux's rtw89_8852b_trk_cfg is used
    TxpwrTrkDefault,
    /// Not Linux (its steps cannot fail): `entry` stopped on `error` (register and value inside)
    /// after it had changed the chip. What it had stopped or saved was put back first: the
    /// scheduler's TX enables, btc's RFK state, the PMAC test packets, the saved BB/KIP/RF
    /// registers. Any [`Rfk2Event::UndoFailed`] of that putting back comes before this event.
    Aborted {
        entry: &'static str,
        error: Rtw89Error,
    },
    /// One step of putting back after a failure failed too (`step` names it); once the chip is
    /// gone the remaining steps are skipped
    UndoFailed {
        step: &'static str,
        error: Rtw89Error,
    },
}

impl fmt::Display for Rfk2Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rfk2Event::OneShotTimeout { path, addr, last } => write!(
                f,
                "[DPK] S{path} one-shot timeout (0x{addr:x} = 0x{last:x})"
            ),
            Rfk2Event::RxModeTimeout { path, mode } => {
                write!(f, "[RFK] S{path} still in RF mode {mode} after 5 ms")
            }
            Rfk2Event::DpkFailed {
                path,
                kidx,
                corr_val,
                dc_i,
                dc_q,
            } => write!(
                f,
                "[DPK] S{path}[{kidx}] DPK fail (corr {corr_val}, dc {dc_i}/{dc_q})"
            ),
            Rfk2Event::DpkSkipped { band } => {
                write!(f, "[DPK] Skip DPK due to ext_PA exist (band {band})")
            }
            Rfk2Event::TssiNoReport { path, power } => write!(
                f,
                "[TSSI PA K] S{path} TSSI report not ready (power {power})"
            ),
            Rfk2Event::TxpwrTrkDefault => {
                write!(f, "no TXPWR_TRK element, rtw89_8852b_trk_cfg used")
            }
            Rfk2Event::Aborted { entry, error } => {
                write!(f, "[RFK] {entry} stopped on {error:?}, chip state put back")
            }
            Rfk2Event::UndoFailed { step, error } => {
                write!(f, "[RFK] putting back failed at {step}: {error:?}")
            }
        }
    }
}

/// The TSSI/DPK part of struct rtw89_dev (JS `d.rfk` without rfk1's parts and `isTssiMode`)
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Rfk2State {
    pub dpk: Dpk,
    pub tssi: Tssi,
    pub fem: Fem,
    /// rtwdev->fw.elm_info.txpwr_trk (`load_txpwr_trk`); `None`: Linux's own table
    pub txpwr_trk: Option<TxpwrTrk>,
    /// soft poll timeouts so far (the JS `pollTimeouts`, for the logs of a chip run)
    pub poll_timeouts: u32,
    pub(crate) events: EventLog<Rfk2Event>,
}

impl Rfk2State {
    pub fn new() -> Rfk2State {
        Rfk2State::default()
    }
    /// What happened since the last call, oldest first
    pub fn take_events(&mut self) -> Vec<Rfk2Event> {
        self.events.take()
    }
    pub fn events(&self) -> &[Rfk2Event] {
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
    pub(crate) fn push(&mut self, e: Rfk2Event) {
        self.events.push(e);
    }
}

/// The neighbours rfk2 calls, as Linux calls them (JS rfk2 imports them from btc.mjs and
/// macinit.mjs)
pub trait Rfk2Hooks<B: Bus + ?Sized> {
    /// rtw89_btc_ntfy_wl_rfk (coex.c:9466): `phy_map` from rtw89_btc_phymap, `typ` BTC_WRFKT_*,
    /// `state` BTC_WRFK_*
    fn btc_wl_rfk(&mut self, bus: &mut B, phy_map: u32, typ: u32, state: u32) -> Rtw89Result<()>;
    /// rtw89_chip_stop_sch_tx (MAC 0, RTW89_SCH_TX_SEL_ALL): returns tx_en
    fn stop_sch_tx(&mut self, bus: &mut B) -> Rtw89Result<u16>;
    /// rtw89_chip_resume_sch_tx (MAC 0)
    fn resume_sch_tx(&mut self, bus: &mut B, tx_en: u16) -> Rtw89Result<()>;
}

/// rfk1's neighbours with rfk2 in them: rfk1's `dpk_init` ... `wifi_scan_notify` run rfk2 on
/// `st`, everything else goes to `hooks` (btc, macinit, fw; its own rfk2 entries are never
/// called). rfk2's btc notification and scheduler stop/resume go to `hooks` as well.
pub struct Rfk2Link<'a, H: ?Sized> {
    pub st: &'a mut Rfk2State,
    pub hooks: &'a mut H,
    /// the channel context's channel (rtw89_chan_get(chanctx 0)): rfk1's scan notification
    /// does not carry it, rtw8852b_wifi_scan_notify needs it (`None`: still zero)
    pub chan: Option<Chan>,
}

/// `H`'s btc and macinit part as rfk2's neighbours
struct Via<'a, H: ?Sized>(&'a mut H);

impl<B: Bus + ?Sized, H: RfkHooks<B> + ?Sized> Rfk2Hooks<B> for Via<'_, H> {
    fn btc_wl_rfk(&mut self, bus: &mut B, phy_map: u32, typ: u32, state: u32) -> Rtw89Result<()> {
        self.0.btc_wl_rfk(bus, phy_map, typ, state)
    }
    fn stop_sch_tx(&mut self, bus: &mut B) -> Rtw89Result<u16> {
        self.0.stop_sch_tx(bus)
    }
    fn resume_sch_tx(&mut self, bus: &mut B, tx_en: u16) -> Rtw89Result<()> {
        self.0.resume_sch_tx(bus, tx_en)
    }
}

impl<B: Bus + ?Sized, H: RfkHooks<B> + ?Sized> RfkHooks<B> for Rfk2Link<'_, H> {
    fn btc_wl_rfk(&mut self, bus: &mut B, phy_map: u32, typ: u32, state: u32) -> Rtw89Result<()> {
        self.hooks.btc_wl_rfk(bus, phy_map, typ, state)
    }
    fn btc_conn_rfk(&mut self, bus: &mut B, start: bool) -> Rtw89Result<()> {
        self.hooks.btc_conn_rfk(bus, start)
    }
    fn btc_preserve_bt_time(&mut self, bus: &mut B, ms: u32) -> Rtw89Result<()> {
        self.hooks.btc_preserve_bt_time(bus, ms)
    }
    fn stop_sch_tx(&mut self, bus: &mut B) -> Rtw89Result<u16> {
        self.hooks.stop_sch_tx(bus)
    }
    fn resume_sch_tx(&mut self, bus: &mut B, tx_en: u16) -> Rtw89Result<()> {
        self.hooks.resume_sch_tx(bus, tx_en)
    }
    fn h2c(&mut self, bus: &mut B, h: &H2c, payload: &[u8]) -> Rtw89Result<()> {
        self.hooks.h2c(bus, h, payload)
    }
    fn dpk_init(&mut self, bus: &mut B, phy: &mut Phy) -> Rtw89Result<()> {
        dpk_init(bus, phy, self.st)
    }
    fn dpk(
        &mut self,
        bus: &mut B,
        phy: &mut Phy,
        chan: Option<&Chan>,
        phy_idx: u32,
    ) -> Rtw89Result<()> {
        dpk(bus, phy, self.st, chan, &mut Via(&mut *self.hooks), phy_idx)
    }
    fn dpk_track(&mut self, bus: &mut B, phy: &mut Phy) -> Rtw89Result<()> {
        dpk_track(bus, phy, self.st)
    }
    fn tssi(
        &mut self,
        bus: &mut B,
        phy: &mut Phy,
        chan: Option<&Chan>,
        phy_idx: u32,
        hwtx_en: bool,
    ) -> Rtw89Result<()> {
        tssi(
            bus,
            phy,
            self.st,
            chan,
            &mut Via(&mut *self.hooks),
            phy_idx,
            hwtx_en,
        )
    }
    fn tssi_scan(
        &mut self,
        bus: &mut B,
        phy: &mut Phy,
        chan: Option<&Chan>,
        phy_idx: u32,
    ) -> Rtw89Result<()> {
        tssi_scan(bus, phy, self.st, chan, phy_idx)
    }
    fn wifi_scan_notify(
        &mut self,
        bus: &mut B,
        phy: &mut Phy,
        start: bool,
        phy_idx: u32,
    ) -> Rtw89Result<()> {
        let chan = self.chan;
        wifi_scan_notify(
            bus,
            phy,
            self.st,
            chan.as_ref(),
            &mut Via(&mut *self.hooks),
            start,
            phy_idx,
        )
    }
}

/// The efuse values TSSI reads (Linux's rtwdev->tssi and ->efuse_gain); refused before any
/// access when the efuse was never parsed (Linux's would be zero, the JS would throw half-way)
fn efuse_of(phy: &Phy) -> Rtw89Result<&Efuse> {
    phy.efuse.as_ref().ok_or(Rtw89Error::Invalid {
        what: "rfk2: TSSI before the efuse was parsed",
        value: 0,
    })
}

/// A PHY index rfk2 can index its per-PHY state with (dbcc off: only 0 is used)
fn phy_ok(phy_idx: u32) -> Rtw89Result<usize> {
    if (phy_idx as usize) < 2 {
        Ok(phy_idx as usize)
    } else {
        Err(Rtw89Error::Invalid {
            what: "rfk2: PHY index past the chip's two",
            value: phy_idx,
        })
    }
}

/// rfk2-common.mjs `s8`: the (s8) conversions of the C
pub(crate) fn s8(v: i32) -> i32 {
    v as i8 as i32
}

/// Linux: sign_extend32 (include/linux/bitops.h)
pub(crate) fn sign_extend32(v: u32, index: u32) -> i32 {
    let sh = 31u32.saturating_sub(index.min(31));
    ((v << sh) as i32) >> sh
}

// Linux: read_poll_timeout_atomic (include/linux/iopoll.h) on rtw89_phy_read32_mask with
// val == x, where Linux only logs a timeout (rfk2-common.mjs pollSoft): one poll op; its
// timeout is counted and noted, any other bus error goes up. B4: the bus bounds the wait.
// `timeout_us` is rounded up to whole milliseconds, at least 1 (the bus waits in ms, as the
// JS); both callers pass whole ms. Not Linux or the JS: a timeout whose last read is all ones
// (the chip fell off the bus; neither polled field can be all ones on a live chip) goes up as
// the bus's PollTimeout error (register and value inside), so DPK stops after one wait instead
// of waiting out every one-shot.
pub fn poll_soft<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut Rfk2State,
    path: u32,
    addr: u32,
    mask: u32,
    x: u32,
    timeout_us: u32,
) -> Rtw89Result<bool> {
    let ms = u16::try_from(timeout_us.div_ceil(1000).max(1)).unwrap_or(u16::MAX);
    match bus.poll32(BB.wrapping_add(addr), mask, prep(mask, x), ms) {
        Ok(()) => Ok(true),
        Err(e @ BusError::PollTimeout { last, .. }) => {
            st.poll_timeouts = st.poll_timeouts.wrapping_add(1);
            st.push(Rfk2Event::OneShotTimeout { path, addr, last });
            if last == u32::MAX {
                Err(e.into())
            } else {
                Ok(false)
            }
        }
        Err(e) => Err(e.into()),
    }
}

// rfk1's _wait_rx_mode / _tmac_tx_pause (rtw8852b_rfk.c:1569, 1587) with rfk2's log
fn wait_rx_mode<B: Bus + ?Sized>(bus: &mut B, st: &mut Rfk2State, kp: u32) -> Rtw89Result<()> {
    let mut log = EventLog::<RfkEvent>::new();
    let r = util::wait_rx_mode(bus, kp, &mut log);
    for e in log.take() {
        if let RfkEvent::RxModeTimeout { path, mode } = e {
            st.push(Rfk2Event::RxModeTimeout { path, mode });
        }
    }
    r
}

/// The tail of a sequence that changed the chip (puts saved registers back, restarts what it
/// stopped). On the good path this runs as the JS: step after step. Not the JS: once a step has
/// failed, before the tail or in it, every later step is still tried (best effort); the first
/// error is the one that goes up, each later failure becomes an [`Rfk2Event::UndoFailed`]. Once
/// an error says the chip is gone, the remaining steps are skipped.
pub(crate) struct Steps<'a> {
    st: &'a mut Rfk2State,
    err: Option<Rtw89Error>,
    gone: bool,
}

impl<'a> Steps<'a> {
    /// The tail after `r`, the result of what it puts back
    pub(crate) fn after(st: &'a mut Rfk2State, r: Rtw89Result<()>) -> Steps<'a> {
        let err = r.err();
        let gone = err.as_ref().is_some_and(chip_gone);
        Steps { st, err, gone }
    }
    /// One step `what` of the tail
    pub(crate) fn step<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        what: &'static str,
        f: impl FnOnce(&mut B) -> Rtw89Result<()>,
    ) {
        if self.gone {
            return;
        }
        if let Err(e) = f(bus) {
            self.gone = chip_gone(&e);
            if self.err.is_none() {
                self.err = Some(e);
            } else {
                self.st.push(Rfk2Event::UndoFailed {
                    step: what,
                    error: e,
                });
            }
        }
    }
    /// The first error of the sequence and its tail
    pub(crate) fn finish(self) -> Rtw89Result<()> {
        self.err.map_or(Ok(()), Err)
    }
}

/// Errors after which no further access can reach the chip: putting back is pointless. A soft
/// poll's all-ones timeout is not among them: should a live chip ever read all ones there, the
/// TX enables and btc must still come back (on a dead chip the few writes cost nothing).
fn chip_gone(e: &Rtw89Error) -> bool {
    matches!(e, Rtw89Error::Bus(BusError::Gone | BusError::Transport))
}

/// `r` of a sequence that started something, then `stop` (see [`Steps`]: it runs after a
/// failure too)
fn then_stop<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut Rfk2State,
    what: &'static str,
    r: Rtw89Result<()>,
    stop: impl FnOnce(&mut B) -> Rtw89Result<()>,
) -> Rtw89Result<()> {
    let mut steps = Steps::after(st, r);
    steps.step(bus, what, stop);
    steps.finish()
}

/// `body` with the scheduler's TX stopped (rtw89_chip_stop_sch_tx ... resume_sch_tx): the TX
/// enables are written back even when `body` fails
fn with_sch_tx_stopped<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut Rfk2State,
    hooks: &mut dyn Rfk2Hooks<B>,
    body: impl FnOnce(&mut B, &mut Rfk2State) -> Rtw89Result<()>,
) -> Rtw89Result<()> {
    let tx_en = hooks.stop_sch_tx(bus)?;
    let r = body(bus, st);
    then_stop(bus, st, "resume_sch_tx", r, |b| {
        hooks.resume_sch_tx(b, tx_en)
    })
}

/// An entry's result after its frame: a failure is noted as [`Rfk2Event::Aborted`] (B6)
fn aborted(st: &mut Rfk2State, entry: &'static str, r: Rtw89Result<()>) -> Rtw89Result<()> {
    if let Err(e) = &r {
        st.push(Rfk2Event::Aborted {
            entry,
            error: e.clone(),
        });
    }
    r
}

// The frame of every entry (JS rfkRun): what the caller sent runs first, and everything rfk2
// sent has run when it returns. Ops run in order as they are issued, so a failure half-way
// leaves nothing queued behind it (the JS had to drop its unsent ops there).
fn run<B: Bus + ?Sized>(
    bus: &mut B,
    body: impl FnOnce(&mut B) -> Rtw89Result<()>,
) -> Rtw89Result<()> {
    bus.flush()?;
    body(bus)?;
    bus.flush()?;
    Ok(())
}

// Linux: rtw8852b_dpk_init (rtw8852b_rfk.c:3739)
pub fn dpk_init<B: Bus + ?Sized>(
    bus: &mut B,
    _phy: &mut Phy,
    st: &mut Rfk2State,
) -> Rtw89Result<()> {
    run(bus, |b| dpkflow::set_dpd_backoff(b, st, RTW89_PHY_0))
}

// Linux: rtw8852b_dpk (rtw8852b_rfk.c:3794). 8852B: dbcc off, so PHY 0 is MAC 0, the one the
// scheduler stop/resume drive (RTW89_SCH_TX_SEL_ALL).
pub fn dpk<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &mut Phy,
    st: &mut Rfk2State,
    chan: Option<&Chan>,
    hooks: &mut dyn Rfk2Hooks<B>,
    phy_idx: u32,
) -> Rtw89Result<()> {
    run(bus, |b| {
        let c = chan_get(chan)?;
        phy_ok(phy_idx)?;
        let phy_map = btc_phymap(chan, phy_idx, 0)?;
        hooks.btc_wl_rfk(b, phy_map, BTC_WRFKT_DPK, BTC_WRFK_START)?;
        let r = with_sch_tx_stopped(b, st, hooks, |b, st| {
            wait_rx_mode(b, st, kpath(phy_idx))?;
            st.dpk.is_dpk_enable = true;
            st.dpk.is_dpk_reload_en = false;
            dpkflow::dpk_run(b, phy, st, phy_idx, false, &c)
        });
        let r = then_stop(b, st, "btc_wl_rfk DPK STOP", r, |b| {
            hooks.btc_wl_rfk(b, phy_map, BTC_WRFKT_DPK, BTC_WRFK_STOP)
        });
        aborted(st, "dpk", r)
    })
}

// Linux: rtw8852b_dpk_track (rtw8852b_rfk.c:3812)
pub fn dpk_track<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &mut Phy,
    st: &mut Rfk2State,
) -> Rtw89Result<()> {
    run(bus, |b| dpkflow::dpk_track_run(b, phy, st))
}

// Linux: rtw8852b_tssi (rtw8852b_rfk.c:3817); the scheduler stop/resume as in `dpk`
pub fn tssi<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &mut Phy,
    st: &mut Rfk2State,
    chan: Option<&Chan>,
    hooks: &mut dyn Rfk2Hooks<B>,
    phy_idx: u32,
    hwtx_en: bool,
) -> Rtw89Result<()> {
    run(bus, |b| {
        let c = chan_get(chan)?;
        efuse_of(phy)?;
        let phy_map = btc_phymap(chan, phy_idx, RF_AB)?;
        hooks.btc_wl_rfk(b, phy_map, BTC_WRFKT_IQK, BTC_WRFK_ONESHOT_START)?;
        let r = tssi_body(b, phy, st, hooks, phy_idx, hwtx_en, &c);
        let r = then_stop(b, st, "btc_wl_rfk IQK ONESHOT_STOP", r, |b| {
            hooks.btc_wl_rfk(b, phy_map, BTC_WRFKT_IQK, BTC_WRFK_ONESHOT_STOP)
        });
        aborted(st, "tssi", r)
    })
}

// rtw8852b_tssi between btc's ONESHOT_START and ONESHOT_STOP
fn tssi_body<B: Bus + ?Sized>(
    b: &mut B,
    phy: &mut Phy,
    st: &mut Rfk2State,
    hooks: &mut dyn Rfk2Hooks<B>,
    phy_idx: u32,
    hwtx_en: bool,
    c: &RfkChan,
) -> Rtw89Result<()> {
    tssi::tssi_disable(b, phy)?;
    for i in RF_PATH_A..RF_PATH_NUM_8852B {
        tssi::tssi_rf_setting(b, i, c)?;
        tssi::tssi_set_sys(b, i, c)?;
        tssi::tssi_ini_txpwr_ctrl_bb(b, i)?;
        tssi::tssi_ini_txpwr_ctrl_bb_he_tb(b, i)?;
        tssi::tssi_set_dck(b, i)?;
        tssi::tssi_set_tmeter_tbl(b, phy, st, i, c)?;
        tssi::tssi_set_dac_gain_tbl(b, i)?;
        tssi::tssi_slope_cal_org(b, i, c)?;
        tssi::tssi_alignment_default(b, i, true, c)?;
        tssi::tssi_set_tssi_slope(b, i)?;
        with_sch_tx_stopped(b, st, hooks, |b, st| {
            wait_rx_mode(b, st, kpath(phy_idx))?; // _tmac_tx_pause(true); false does nothing
            if hwtx_en {
                alimk::tssi_alimentk(b, phy, st, i, c)?;
            }
            Ok(())
        })?;
    }
    tssi::tssi_enable(b, phy)?;
    tssi::tssi_set_efuse_to_de(b, phy, c)
}

// Linux: rtw8852b_tssi_scan (rtw8852b_rfk.c:3856)
pub fn tssi_scan<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &mut Phy,
    st: &mut Rfk2State,
    chan: Option<&Chan>,
    _phy_idx: u32,
) -> Rtw89Result<()> {
    run(bus, |b| {
        let c = chan_get(chan)?;
        efuse_of(phy)?;
        let band = tssi::tssi_alimk_band(c.channel) as usize;
        tssi::tssi_disable(b, phy)?;
        for i in RF_PATH_A..RF_PATH_NUM_8852B {
            tssi::tssi_rf_setting(b, i, &c)?;
            tssi::tssi_set_sys(b, i, &c)?;
            tssi::tssi_set_tmeter_tbl(b, phy, st, i, &c)?;
            if st.tssi.alignment_done[i as usize][band] {
                tssi::tssi_alimentk_done(b, st, i, &c)?;
            } else {
                tssi::tssi_alignment_default(b, i, true, &c)?;
            }
        }
        tssi::tssi_enable(b, phy)?;
        tssi::tssi_set_efuse_to_de(b, phy, &c)
    })
}

// Linux: rtw8852b_tssi_default_txagc (rtw8852b_rfk.c:3895); the reads of 0x5818/0x7818 for the
// log are left out (as the JS)
fn tssi_default_txagc<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &mut Phy,
    st: &mut Rfk2State,
    chan: Option<&Chan>,
    hooks: &mut dyn Rfk2Hooks<B>,
    phy_idx: u32,
    enable: bool,
) -> Rtw89Result<()> {
    let c = chan_get(chan)?;
    if enable {
        if !phy.tssi_mode[0] && !phy.tssi_mode[1] {
            tssi(bus, phy, st, chan, hooks, phy_idx, true)?;
        }
        return Ok(());
    }
    phy_mask32(bus, R_P0_TSSI_TRK, B_P0_TSSI_OFT, 0xc0)?;
    phy_mask32(bus, R_P1_TSSI_TRK, B_P1_TSSI_OFT, 0xc0)?;
    phy_mask32(bus, R_P0_TSSI_TRK, B_P0_TSSI_OFT_EN, 0x0)?;
    phy_mask32(bus, R_P0_TSSI_TRK, B_P0_TSSI_OFT_EN, 0x1)?;
    phy_mask32(bus, R_P1_TSSI_TRK, B_P1_TSSI_OFT_EN, 0x0)?;
    phy_mask32(bus, R_P1_TSSI_TRK, B_P1_TSSI_OFT_EN, 0x1)?;
    tssi::tssi_alimentk_done(bus, st, RF_PATH_A, &c)?;
    tssi::tssi_alimentk_done(bus, st, RF_PATH_B, &c)
}

// Linux: rtw8852b_wifi_scan_notify (rtw8852b_rfk.c:3937); `chan` is chanctx 0's
pub fn wifi_scan_notify<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &mut Phy,
    st: &mut Rfk2State,
    chan: Option<&Chan>,
    hooks: &mut dyn Rfk2Hooks<B>,
    start: bool,
    phy_idx: u32,
) -> Rtw89Result<()> {
    run(bus, |b| {
        tssi_default_txagc(b, phy, st, chan, hooks, phy_idx, start)
    })
}

/// The channel's band as phy's [`crate::phy::Band`] (the BB helpers take it)
pub(crate) fn band_of(c: &RfkChan) -> crate::phy::Band {
    if c.band_type == RTW89_BAND_5G {
        crate::phy::Band::B5g
    } else {
        crate::phy::Band::B2g
    }
}
