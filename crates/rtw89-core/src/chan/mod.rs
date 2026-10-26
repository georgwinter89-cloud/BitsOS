//! Channel switch and TX power of the RTL8852BE as Linux rtw89 does them for one channel
//! context, 20 MHz, station mode (JS js/chan.mjs with chan-bb/-txpwr/-path/-fw/-efuse/-regd.mjs):
//! core.c __rtw89_set_channel with rtw8852b.c set_channel / set_channel_help and the
//! rtw8852b_common.c MAC, BB and TX power parts; the chip ops phy's dm init calls
//! (init_txpwr_unit, set_txpwr_ctrl, power_trim, cfg_txrx_path); the regulatory domain (DE by
//! default) and the channel flags of the country. Each function keeps the JS driver's (=
//! Linux's) order of register accesses and names its Linux original.
//!
//! The state is [`ChanState`] (JS `d.chan`: struct rtw89_chan of the channel context, struct
//! rtw89_chan_rcd, the entity state, the country and the firmware's TX power tables). The
//! efuse values (gain offsets, gain comp, power trim; JS chan-efuse.mjs) and the BB gain table
//! are phy's ([`Phy`]). The neighbours of the switch (macinit's scheduler TX stop/resume,
//! rfk1's RF channel set, btc's band switch, rfk1/rfk2's band-change calibration) are
//! [`ChanHooks`]; [`ChanLink`] builds them from rfk1's and the caller's parts. Linux's warnings
//! and the switch's state changes are [`ChanEvent`]s the caller drains (B6).

pub mod bb;
pub mod fw;
pub mod path;
pub mod regd;
pub mod txpwr;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_hostile;

use alloc::vec::Vec;
use core::fmt;

use crate::bus::{Bus, BusError};
use crate::error::{Rtw89Error, Rtw89Result};
use crate::events::EventLog;
use crate::fw::Firmware;
use crate::phy::bbx::bb_reset_en;
use crate::phy::io::rf_w;
use crate::phy::{Band, Chan, Phy};
use crate::rfk1::{self, RfkHooks, RfkState};
use crate::tables::regs::*;

pub use fw::{ByrHead, RateDesc, Regd, RegdMap, S8Table, TxpwrConf, TxpwrTables};
pub use regd::{ChanFlags, DEFAULT_COUNTRY, channel_flags};

/// struct rtw89_chan of the one channel context BitsOS has (20 MHz, primary = center channel)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtwChan {
    pub band: Band,
    pub channel: u32,
    pub primary_channel: u32,
    /// enum rtw89_bandwidth
    pub band_width: u32,
    pub freq: u32,
    /// enum rtw89_subband
    pub subband_type: u32,
    /// enum rtw89_tx_comp_band
    pub tx_comp_band: u32,
    pub pri_ch_idx: u32,
    pub pri_sb_idx: u32,
    pub rfsi_band: u32,
}

impl RtwChan {
    /// hal->chanctx[0]'s chan before the first switch: zeroed (2 GHz, channel 0), as when
    /// cfg_txrx_path runs in phy_dm_init
    pub const ZERO: RtwChan = RtwChan {
        band: Band::B2g,
        channel: 0,
        primary_channel: 0,
        band_width: RTW89_CHANNEL_WIDTH_20,
        freq: 0,
        subband_type: RTW89_CH_2G,
        tx_comp_band: 0,
        pri_ch_idx: 0,
        pri_sb_idx: 0,
        rfsi_band: 0,
    };

    /// enum rtw89_band of `band`
    pub fn band_type(&self) -> u32 {
        band_type(self.band)
    }

    /// The channel as phy and the RF calibrations take it (`None` while it is still zero)
    pub fn phy_chan(&self) -> Option<Chan> {
        (self.channel != 0).then_some(Chan {
            band: self.band,
            ch: self.channel,
            bw: 20,
        })
    }
}

impl Default for RtwChan {
    fn default() -> RtwChan {
        RtwChan::ZERO
    }
}

fn band_type(b: Band) -> u32 {
    match b {
        Band::B2g => RTW89_BAND_2G,
        Band::B5g => RTW89_BAND_5G,
        Band::B6g => 2, // RTW89_BAND_6G (enum rtw89_band)
    }
}

// Linux: rtw89_chan_create (chan.c:131) for 20 MHz, primary = center, with
// ieee80211_channel_to_frequency (net/wireless/util.c, not in linux/), rtw89_get_subband_type
// (chan.c:19) and rtw89_get_tx_comp_band
pub fn chan_create(band: Band, ch: u32) -> RtwChan {
    let is2g = band == Band::B2g;
    let mhz = |base: u32| ch.saturating_mul(5).saturating_add(base);
    let freq = if is2g {
        if ch == 14 { 2484 } else { mhz(2407) }
    } else {
        mhz(5000)
    };
    RtwChan {
        band,
        channel: ch,
        primary_channel: ch,
        band_width: RTW89_CHANNEL_WIDTH_20,
        freq,
        subband_type: rfk1::util::get_subband_type(band_type(band), ch),
        tx_comp_band: if is2g {
            RTW89_TX_COMP_BAND_2GHZ
        } else if ch < 149 {
            RTW89_TX_COMP_BAND_5GHZ_L
        } else {
            RTW89_TX_COMP_BAND_5GHZ_H
        },
        pri_ch_idx: RTW89_SC_DONT_CARE,
        pri_sb_idx: 0,
        rfsi_band: if is2g {
            RFSI_CTRL_BAND_2GHZ
        } else {
            RFSI_CTRL_BAND_5_6GHZ
        },
    }
}

/// What Linux only warns about in the channel part, and the state changes of the switch and the
/// country, for the caller's log (B6)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChanEvent {
    /// rtw89_pci_ops_read32_cmac: a CMAC register read 0xDEADBEEF (its clock off); R_AX_CK_EN
    /// all on and read again `tries` times; `value` is the last read
    CmacRetry { addr: u32, tries: u32, value: u32 },
    /// rtw89_mac_cfg_ppdu_status_ax: the CMAC is not enabled (R_AX_CMAC_FUNC_EN read `value`):
    /// the PPDU status was left as it is
    PpduStatusSkipped { enable: bool, value: u32 },
    /// rtw89_mac_get_txpwr_cr_ax: "[TXPWR] check txpwr cr 0x%x(phy0) fail"; not written
    TxpwrCrRefused { addr: u32 },
    /// rtw89_recognize_regd_from_elm: "regd element ents (%d) are over max num" (ignored)
    RegdElementIgnored { num_ents: u32 },
    /// rtw89_regd_notifier_apply: the country's TX power regulation per band (2, 5, 6 GHz)
    RegdSet {
        alpha2: [u8; 2],
        txpwr_regd: [u32; 3],
    },
    /// The channel context is on this channel now (`band_changed`: the band-change steps ran)
    Switched {
        band: Band,
        ch: u32,
        prev_ch: u32,
        band_changed: bool,
    },
    /// Not Linux (its switch cannot fail): `entry` stopped on `error` (register and value
    /// inside) after it had changed the chip; the TX power and the help's leave were still
    /// tried, the entity is inactive (the next switch redoes the band-change steps). Any
    /// [`ChanEvent::UndoFailed`] of that tail comes before this event.
    Aborted {
        entry: &'static str,
        error: Rtw89Error,
    },
    /// One step of the tail after a failure failed too (`step` names it); once the chip is gone
    /// the remaining steps are skipped
    UndoFailed {
        step: &'static str,
        error: Rtw89Error,
    },
}

impl fmt::Display for ChanEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChanEvent::CmacRetry { addr, tries, value } => write!(
                f,
                "CMAC 0x{addr:x} read 0xdeadbeef, CMAC clocks on, {tries} more reads (0x{value:x})"
            ),
            ChanEvent::PpduStatusSkipped { enable, value } => write!(
                f,
                "CMAC off (0x{value:x}): PPDU status {} skipped",
                if *enable { "on" } else { "off" }
            ),
            ChanEvent::TxpwrCrRefused { addr } => {
                write!(f, "[TXPWR] check txpwr cr 0x{addr:x}(phy0) fail")
            }
            ChanEvent::RegdElementIgnored { num_ents } => write!(
                f,
                "regd element ents ({num_ents}) are over max num (255): ignored"
            ),
            ChanEvent::RegdSet { alpha2, txpwr_regd } => write!(
                f,
                "regd {}{}: txpwr regulation {:?}",
                char::from(alpha2[0]),
                char::from(alpha2[1]),
                txpwr_regd
            ),
            ChanEvent::Switched {
                band,
                ch,
                prev_ch,
                band_changed,
            } => write!(
                f,
                "channel {ch} ({band:?}), was {prev_ch}{}",
                if *band_changed { ", band changed" } else { "" }
            ),
            ChanEvent::Aborted { entry, error } => write!(f, "{entry} stopped: {error:?}"),
            ChanEvent::UndoFailed { step, error } => {
                write!(f, "after a failure, {step} failed too: {error:?}")
            }
        }
    }
}

/// JS `d.chan`: the channel context (struct rtw89_chan and struct rtw89_chan_rcd), the entity
/// state, the country and the firmware's TX power tables
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChanState {
    pub chan: RtwChan,
    /// struct rtw89_chan_rcd: prev_primary_channel, the previous band, prev_band_type,
    /// band_changed (rtw89_assign_entity_chan)
    pub prev_ch: u32,
    pub prev_band: Band,
    pub prev_band_type: u32,
    pub band_changed: bool,
    /// rtw89_get_entity_state(PHY 0): a switch went through since the last power-on
    pub entity_active: bool,
    /// phy_dm_init's cfg_txrx_path ran since the last power-on (not Linux: the order guard)
    pub dm_init_done: bool,
    /// the country's regulation (rtw89_regd_notifier_apply); `None` until `load`
    pub regd: Option<Regd>,
    /// the REGD element's map (rtw89_regd_setup with the element)
    pub regd_map: Option<Vec<Regd>>,
    /// rtwdev->rfe_parms' tables (`load`)
    pub tables: Option<TxpwrTables>,
    events: EventLog<ChanEvent>,
}

impl Default for ChanState {
    // before the first switch: a zeroed rtw89_chan and rtw89_chan_rcd (2 GHz, channel 0)
    fn default() -> ChanState {
        ChanState {
            chan: RtwChan::ZERO,
            prev_ch: 0,
            prev_band: Band::B2g,
            prev_band_type: RTW89_BAND_2G,
            band_changed: false,
            entity_active: false,
            dm_init_done: false,
            regd: None,
            regd_map: None,
            tables: None,
            events: EventLog::default(),
        }
    }
}

impl ChanState {
    pub fn new() -> ChanState {
        ChanState::default()
    }
    /// What happened since the last call, oldest first
    pub fn take_events(&mut self) -> Vec<ChanEvent> {
        self.events.take()
    }
    pub fn events(&self) -> &[ChanEvent] {
        self.events.as_slice()
    }
    /// Events lost to the log's cap so far (never reset; WL-4 drains after every call)
    pub fn events_dropped(&self) -> u64 {
        self.events.dropped()
    }
    /// How many undrained events are kept (default [`crate::events::DEFAULT_EVENT_CAP`])
    pub fn set_event_cap(&mut self, cap: usize) {
        self.events.set_cap(cap);
    }
    pub(crate) fn push(&mut self, e: ChanEvent) {
        self.events.push(e);
    }

    // What Linux sets up at probe and the channel switch reads (JS chan.mjs `setup`, there on
    // the first call that needs it): rtw89_regd_setup (regd.c:709) with the REGD element's map
    // and the default country applied (rtw89_regd_notifier_apply, regd.c:882, without the TX
    // power: nothing is switched yet), and rtw89_core_setup_rfe_parms (core.c:7184) with the
    // firmware's TX power tables for `rfe_type` (the efuse's) and `hal_aid` (0 on AX chips).
    // A file without the REGD element or a needed TX power element is an error (`FwFile`);
    // nothing is kept then.
    pub fn load(&mut self, fw: &Firmware, rfe_type: u8, hal_aid: u16) -> Rtw89Result<()> {
        let m = fw::load_regd_map(fw)?;
        for n in &m.ignored {
            self.push(ChanEvent::RegdElementIgnored { num_ents: *n });
        }
        let map = m.map.ok_or(Rtw89Error::FwFile {
            what: "firmware lacks the REGD element",
            value: RTW89_FW_ELEMENT_ID_REGD,
        })?;
        let tables = fw::load_rfe_data(fw, rfe_type, hal_aid)?;
        self.regd_map = Some(map);
        if self.regd.is_none() {
            regd_apply(self, DEFAULT_COUNTRY)?;
        }
        self.tables = Some(tables);
        Ok(())
    }

    /// cfg80211's flags of a channel in the current country (DE until set)
    pub fn channel_flags(&self, band: Band, ch: u32) -> ChanFlags {
        regd::channel_flags(self.regd.as_ref(), band, ch)
    }
}

// Linux: rtw89_regd_notifier_apply (regd.c:882) with rtw89_regd_find_reg_by_name (regd.c:307):
// the country's TX power regulation; one the map does not have gets the worldwide one. The
// REGULATORY_COUNTRY_IE_IGNORE flag is cfg80211's (no register).
pub fn regd_apply(st: &mut ChanState, alpha2: [u8; 2]) -> Rtw89Result<Regd> {
    let map = st.regd_map.as_ref().ok_or(Rtw89Error::Invalid {
        what: "chan: country before the REGD element is loaded",
        value: 0,
    })?;
    let r = regd::find_regd(map, alpha2);
    st.regd = Some(r);
    st.push(ChanEvent::RegdSet {
        alpha2: r.alpha2,
        txpwr_regd: r.txpwr_regd,
    });
    Ok(r)
}

// Linux: rtw89_regd_notifier (regd.c:899): the country's regulation, then the TX power again
// (regd.c:921). The UNII-4, 6 GHz, TAS and antenna gain policies have nothing to apply on BitsOS;
// rtw89_leave_ps_mode: BitsOS never enters power save.
pub fn regd_set<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut ChanState,
    alpha2: [u8; 2],
) -> Rtw89Result<Regd> {
    let r = regd_apply(st, alpha2)?;
    set_txpwr(bus, st)?;
    Ok(r)
}

/// The neighbours of the channel switch, as Linux calls them (JS chan.mjs imports them from
/// macinit.mjs, rfk1.mjs and btc.mjs). [`ChanLink`] has rfk1's part and the RfkHooks' MAC side.
pub trait ChanHooks<B: Bus + ?Sized> {
    /// rtw89_mac_stop_sch_tx(RTW89_SCH_TX_SEL_ALL), MAC 0: returns tx_en
    fn stop_sch_tx(&mut self, bus: &mut B) -> Rtw89Result<u16>;
    /// rtw89_mac_resume_sch_tx, MAC 0
    fn resume_sch_tx(&mut self, bus: &mut B, tx_en: u16) -> Rtw89Result<()>;
    /// rtw8852b_set_channel_rf (rfk1)
    fn set_channel_rf(&mut self, bus: &mut B, chan: &Chan) -> Rtw89Result<()>;
    /// rtw89_btc_ntfy_switch_band (coex.c:8837), btc's
    fn btc_switch_band(&mut self, bus: &mut B, band: Band) -> Rtw89Result<()>;
    /// rtw8852b_rfk_band_changed (rfk1 -> rfk2's TSSI scan)
    fn rfk_band_changed(&mut self, bus: &mut B, phy: &mut Phy, chan: &Chan) -> Rtw89Result<()>;
    /// rtw8852b_rfk_channel (rfk1 with rfk2's TSSI and DPK): Linux's sta_link_add, here only
    /// when the caller of `set_channel` asks for it
    fn rfk_channel(&mut self, bus: &mut B, phy: &mut Phy, chan: &Chan) -> Rtw89Result<()>;
}

/// The hooks from rfk1's state and an [`RfkHooks`] (usually rfk2's `Rfk2Link` around the MAC
/// side and btc): the scheduler TX stop/resume go to the RfkHooks' own, the RF parts to rfk1,
/// the band switch to btc's `btc_switch_band`
pub struct ChanLink<'a, B: Bus + ?Sized> {
    pub rfk: &'a mut RfkState,
    pub hooks: &'a mut dyn RfkHooks<B>,
    /// rtw89_btc_ntfy_switch_band (btc's `btc::switch_band`): it gets the RfkHooks for its
    /// H2Cs (`btc::ViaRfk`)
    pub btc_switch_band: &'a mut BtcBandSwitch<'a, B>,
}

/// btc's band switch inside the channel switch, with the neighbours' H2C path
pub type BtcBandSwitch<'a, B> =
    dyn FnMut(&mut B, Band, &mut dyn RfkHooks<B>) -> Rtw89Result<()> + 'a;

impl<B: Bus + ?Sized> ChanHooks<B> for ChanLink<'_, B> {
    fn stop_sch_tx(&mut self, bus: &mut B) -> Rtw89Result<u16> {
        self.hooks.stop_sch_tx(bus)
    }
    fn resume_sch_tx(&mut self, bus: &mut B, tx_en: u16) -> Rtw89Result<()> {
        self.hooks.resume_sch_tx(bus, tx_en)
    }
    fn set_channel_rf(&mut self, bus: &mut B, chan: &Chan) -> Rtw89Result<()> {
        rfk1::set_channel_rf(bus, self.rfk, Some(chan), RTW89_PHY_0)
    }
    fn btc_switch_band(&mut self, bus: &mut B, band: Band) -> Rtw89Result<()> {
        (self.btc_switch_band)(bus, band, &mut *self.hooks)
    }
    fn rfk_band_changed(&mut self, bus: &mut B, phy: &mut Phy, chan: &Chan) -> Rtw89Result<()> {
        rfk1::rfk_band_changed(bus, phy, Some(chan), self.hooks)
    }
    fn rfk_channel(&mut self, bus: &mut B, phy: &mut Phy, chan: &Chan) -> Rtw89Result<()> {
        rfk1::rfk_channel(bus, phy, self.rfk, Some(chan), self.hooks, RTW89_PHY_0)
    }
}

// Linux: rtw89_pci_ops_read32_cmac (pci.c:1998): a CMAC register (ACCESS_CMAC, mac.h:587) that
// reads 0xDEADBEEF has its clock off: all CMAC clocks on, read again, up to MAC_REG_POOL_COUNT
// times
fn cmac_r32<B: Bus + ?Sized>(bus: &mut B, addr: u32, st: &mut ChanState) -> Rtw89Result<u32> {
    let mut v = bus.r32(addr)?;
    let mut n = 0;
    while v == RTW89_R32_DEAD && n < MAC_REG_POOL_COUNT {
        bus.w32(R_AX_CK_EN, B_AX_CMAC_ALLCKEN)?;
        v = bus.r32(addr)?;
        n += 1;
    }
    if n > 0 {
        st.push(ChanEvent::CmacRetry {
            addr,
            tries: n,
            value: v,
        });
    }
    Ok(v)
}

// Linux: rtw89_write8_set / _clr (core.h:8130, 8157) on a CMAC register: rtw89_pci_ops_read8
// (pci.c:2018) reads the aligned dword through read32_cmac, the write is a byte
fn cmac_rmw8<B: Bus + ?Sized>(
    bus: &mut B,
    addr: u32,
    clr: u32,
    set: u32,
    st: &mut ChanState,
) -> Rtw89Result<()> {
    let v = cmac_r32(bus, addr & !3, st)? >> ((addr & 3) * 8);
    bus.w8(addr, ((v & !clr) | set) & 0xff)?;
    Ok(())
}

// Linux: rtw89_mac_cfg_ppdu_status_ax (mac.c:6290), MAC 0, with rtw89_mac_check_mac_en_ax
// (mac.c:62) for the CMAC through read32_cmac. macinit has the same steps for the core start's
// tail; this copy keeps the JS's CMAC read (with the 0xDEADBEEF retry) and its quiet skip (an
// event here) when the CMAC is off.
fn cfg_ppdu_status<B: Bus + ?Sized>(
    bus: &mut B,
    enable: bool,
    st: &mut ChanState,
) -> Rtw89Result<()> {
    let v = cmac_r32(bus, R_AX_CMAC_FUNC_EN, st)?;
    if v == RTW89_R32_EA || v == RTW89_R32_DEAD || v & B_AX_CMAC_EN == 0 {
        st.push(ChanEvent::PpduStatusSkipped { enable, value: v });
        return Ok(());
    }
    if !enable {
        bus.clr32(R_AX_PPDU_STAT, B_AX_PPDU_STAT_RPT_EN)?;
        return Ok(());
    }
    bus.w32(
        R_AX_PPDU_STAT,
        B_AX_PPDU_STAT_RPT_EN
            | B_AX_APP_MAC_INFO_RPT
            | B_AX_APP_PLCP_HDR_RPT
            | B_AX_PPDU_STAT_RPT_CRC32,
    )?;
    bus.mask32(
        R_AX_HW_RPT_FWD,
        B_AX_FWD_PPDU_STAT_MASK,
        RTW89_PRPT_DEST_HOST,
    )?;
    Ok(())
}

// Linux: rtw8852b_set_channel_help (rtw8852b.c:675), entering, after its rtw89_mac_stop_sch_tx
// (the caller's): PPDU status off, TSSI tracking off, ADC FIFO reset, 40 us, BB reset on
fn help_enter_bb<B: Bus + ?Sized>(bus: &mut B, band: Band, st: &mut ChanState) -> Rtw89Result<()> {
    cfg_ppdu_status(bus, false, st)?;
    bb::tssi_cont_en(bus, false)?;
    bb::adc_en(bus, false)?;
    bus.delay_us(40)?;
    bb_reset_en(bus, band, false) // RTW89_PHY_0
}

// Linux: rtw8852b_set_channel_help (rtw8852b.c:675) with enter = true, MAC 0 / PHY 0, `band` the
// new channel's: the scheduler's TX stopped (returns what rtw89_mac_stop_sch_tx read, for the
// leave), PPDU status off, TSSI tracking off, ADC FIFO reset, 40 us, BB reset on
pub fn help_enter<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut ChanState,
    band: Band,
    hooks: &mut dyn ChanHooks<B>,
) -> Rtw89Result<u16> {
    let tx_en = hooks.stop_sch_tx(bus)?;
    help_enter_bb(bus, band, st)?;
    Ok(tx_en)
}

// rtw8852b_set_channel_help's leave, step by step on `tail`
fn help_leave_on<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut ChanState,
    band: Band,
    hooks: &mut dyn ChanHooks<B>,
    tx_en: u16,
    tail: &mut Tail,
) {
    tail.step(bus, st, "ppdu status on", |b, s| {
        cfg_ppdu_status(b, true, s)
    });
    tail.step(bus, st, "adc on", |b, _| bb::adc_en(b, true));
    tail.step(bus, st, "tssi tracking on", |b, _| {
        bb::tssi_cont_en(b, true)
    });
    tail.step(bus, st, "bb reset off", |b, _| bb_reset_en(b, band, true));
    tail.step(bus, st, "resume sch tx", |b, _| {
        hooks.resume_sch_tx(b, tx_en)
    });
}

// Linux: rtw8852b_set_channel_help (rtw8852b.c:675) with enter = false: PPDU status on, ADC
// FIFO out of reset, TSSI tracking on, BB reset off, the scheduler's TX enables `tx_en` back.
// Not Linux: every step is tried even when one before failed (the first error is returned,
// later ones are `UndoFailed` events).
pub fn help_leave<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut ChanState,
    band: Band,
    hooks: &mut dyn ChanHooks<B>,
    tx_en: u16,
) -> Rtw89Result<()> {
    let mut tail = Tail::after(Ok(()));
    help_leave_on(bus, st, band, hooks, tx_en, &mut tail);
    tail.err.map_or(Ok(()), Err)
}

// Linux: __rtw8852bx_set_channel_mac (rtw8852b_common.c:451), MAC 0, 20 MHz
pub fn set_channel_mac<B: Bus + ?Sized>(
    bus: &mut B,
    chan: &RtwChan,
    st: &mut ChanState,
) -> Rtw89Result<()> {
    let chk = R_AX_TXRATE_CHK;
    let bits = B_AX_CHECK_CCK_EN | B_AX_RTS_LIMIT_IN_OFDM6;
    cmac_rmw8(bus, R_AX_WMAC_RFMOD, B_AX_WMAC_RFMOD_MASK, 0, st)?;
    bus.w32(R_AX_TX_SUB_CARRIER_VALUE, 0)?;
    if chan.channel > 14 {
        cmac_rmw8(bus, chk, B_AX_BAND_MODE, 0, st)?;
        cmac_rmw8(bus, chk, 0, bits, st)?;
    } else {
        cmac_rmw8(bus, chk, 0, B_AX_BAND_MODE, st)?;
        cmac_rmw8(bus, chk, bits, 0, st)?;
    }
    Ok(())
}

/// The tail after a failed step (as rfk2's): every later step is still tried, the first error
/// goes up, each later failure is an [`ChanEvent::UndoFailed`]; once the chip is gone the rest
/// is skipped
struct Tail {
    err: Option<Rtw89Error>,
    gone: bool,
}

fn chip_gone(e: &Rtw89Error) -> bool {
    matches!(e, Rtw89Error::Bus(BusError::Gone | BusError::Transport))
}

impl Tail {
    fn after(r: Rtw89Result<()>) -> Tail {
        let err = r.err();
        let gone = err.as_ref().is_some_and(chip_gone);
        Tail { err, gone }
    }
    fn step<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        st: &mut ChanState,
        what: &'static str,
        f: impl FnOnce(&mut B, &mut ChanState) -> Rtw89Result<()>,
    ) {
        if self.gone {
            return;
        }
        if let Err(e) = f(bus, st) {
            self.gone = chip_gone(&e);
            if self.err.is_none() {
                self.err = Some(e);
            } else {
                st.push(ChanEvent::UndoFailed {
                    step: what,
                    error: e,
                });
            }
        }
    }
}

/// A channel the caller asks `set_channel` for
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChanReq {
    pub band: Band,
    pub ch: u32,
    /// MHz; only 20 is ported
    pub bw: u32,
}

// Linux: __rtw89_set_channel (core.c:568) for one channel context (PHY 0, MAC 0), with
// rtw89_assign_entity_chan (chan.c:199) before it: set_channel_help enter, rtw8852b_set_channel
// (rtw8852b.c:628: MAC, BB, RF), the chip's set_txpwr, set_channel_help leave, then, when the
// entity was not active or the band changed, rtw89_btc_ntfy_switch_band and
// rtw8852b_rfk_band_changed. `rfk` adds rtw89_chip_rfk_channel afterwards: Linux runs that once
// per connection in rtw89_core_sta_link_add (core.c:5765), not per switch.
//
// Refused before any access: another bandwidth than 20 MHz, 6 GHz, no tables (`load`), a channel
// the country does not allow, a switch before phy_dm_init's cfg_txrx_path (since the last
// power-on), no efuse. Not Linux (its steps cannot fail): when a step fails, the TX power and
// the help's leave are still tried step by step (the JS ran them too, the leave as one block),
// the entity is marked inactive and the first error is returned; a failure inside the help's
// enter skips the MAC/BB/RF part (the JS threw there and left the scheduler stopped).
pub fn set_channel<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &mut Phy,
    st: &mut ChanState,
    req: ChanReq,
    hooks: &mut dyn ChanHooks<B>,
    rfk: bool,
) -> Rtw89Result<()> {
    if req.bw != 20 {
        return Err(Rtw89Error::Invalid {
            what: "chan: only 20 MHz channels are ported",
            value: req.bw,
        });
    }
    if req.band == Band::B6g {
        return Err(Rtw89Error::Invalid {
            what: "chan: 6 GHz is never used",
            value: req.ch,
        });
    }
    if st.tables.is_none() || st.regd.is_none() {
        return Err(Rtw89Error::Invalid {
            what: "chan: TX power tables not loaded (ChanState::load)",
            value: 0,
        });
    }
    if !st.channel_flags(req.band, req.ch).allowed {
        return Err(Rtw89Error::Invalid {
            what: "chan: channel not allowed in this country",
            value: req.ch,
        });
    }
    // Linux switches only after rtw89_phy_dm_init (core.c:6697), which ends with cfg_txrx_path;
    // set_gain_offset needs the gain bases its bb_sethw read
    if !st.dm_init_done || phy.efuse.is_none() {
        return Err(Rtw89Error::Invalid {
            what: "chan: set_channel before phy_dm_init (cfg_txrx_path)",
            value: req.ch,
        });
    }
    let new = chan_create(req.band, req.ch);
    let was_active = st.entity_active;
    // rtw89_assign_entity_chan (chan.c:208-213): struct rtw89_chan_rcd, then the new chan
    st.prev_ch = st.chan.primary_channel;
    st.prev_band = st.chan.band;
    st.prev_band_type = st.chan.band_type();
    st.band_changed = new.band_type() != st.chan.band_type();
    st.chan = new;
    let pc = Chan {
        band: req.band,
        ch: req.ch,
        bw: 20,
    };

    let tx_en = match hooks.stop_sch_tx(bus) {
        Ok(v) => v,
        Err(e) => {
            st.entity_active = false;
            st.push(ChanEvent::Aborted {
                entry: "set_channel",
                error: e.clone(),
            });
            return Err(e);
        }
    };
    let entered = help_enter_bb(bus, new.band, st);
    let main = match entered {
        // rtw8852b_set_channel (rtw8852b.c:628): MAC, BB, RF
        Ok(()) => set_channel_mac(bus, &new, st)
            .and_then(|()| bb::set_channel_bb(bus, phy, &new))
            .and_then(|()| hooks.set_channel_rf(bus, &pc)),
        e => e,
    };
    // Linux goes on to set_txpwr and set_channel_done whatever happened (core.c:582-588)
    let mut tail = Tail::after(main);
    tail.step(bus, st, "set txpwr", |b, s| set_txpwr_now(b, s));
    help_leave_on(bus, st, new.band, hooks, tx_en, &mut tail); // rtw8852b_set_channel_help, leaving
    if let Some(e) = tail.err {
        st.entity_active = false; // the next switch redoes the band-change steps
        st.push(ChanEvent::Aborted {
            entry: "set_channel",
            error: e.clone(),
        });
        let _ = bus.flush();
        return Err(e);
    }
    if !was_active || st.band_changed {
        // rtw89_btc_ntfy_switch_band (coex.c:8837), rtw8852b_rfk_band_changed -> tssi_scan; on
        // a failure the entity is marked inactive, so the next switch runs them again (also
        // when it was active: st.chan holds the new band, band_changed would not fire again)
        let r = hooks
            .btc_switch_band(bus, req.band)
            .and_then(|()| hooks.rfk_band_changed(bus, phy, &pc));
        if r.is_err() {
            st.entity_active = false;
        }
        noted(st, "set_channel band change", r)?;
    }
    st.entity_active = true;
    st.push(ChanEvent::Switched {
        band: req.band,
        ch: req.ch,
        prev_ch: st.prev_ch,
        band_changed: st.band_changed,
    });
    if rfk {
        let r = hooks.rfk_channel(bus, phy, &pc);
        noted(st, "rfk_channel", r)?;
    }
    bus.flush()?;
    Ok(())
}

// `r`, with an `Aborted` event when it failed (B6)
fn noted(st: &mut ChanState, entry: &'static str, r: Rtw89Result<()>) -> Rtw89Result<()> {
    if let Err(e) = &r {
        st.push(ChanEvent::Aborted {
            entry,
            error: e.clone(),
        });
    }
    r
}

// the chip's set_txpwr for the current channel (tables and country checked by the callers)
fn set_txpwr_now<B: Bus + ?Sized>(bus: &mut B, st: &mut ChanState) -> Rtw89Result<()> {
    let ChanState {
        chan,
        regd,
        tables,
        events,
        ..
    } = st;
    let (Some(t), Some(r)) = (tables.as_ref(), regd.as_ref()) else {
        return Err(Rtw89Error::Invalid {
            what: "chan: TX power tables not loaded (ChanState::load)",
            value: 0,
        });
    };
    txpwr::set_txpwr_chip(bus, t, r, chan, events)
}

// Linux: rtw89_mac_power_switch (mac.c:1521), off: rtw89_set_entity_state(PHY_0, false)
// (mac.c:1581). After the next power-on phy_dm_init has to run again before a switch. The
// caller calls it wherever it powers the MAC off (macinit::mac_pwr_off, Bringup::stop).
pub fn power_off(st: &mut ChanState) {
    st.entity_active = false;
    st.dm_init_done = false;
}

// Linux: __rtw89_core_set_chip_txpwr (core.c:462) for PHY 0: the TX power again for the current
// channel, e.g. after the regulatory domain changed (regd.c:921); nothing while the entity is
// inactive (core.c:470)
pub fn set_txpwr<B: Bus + ?Sized>(bus: &mut B, st: &mut ChanState) -> Rtw89Result<()> {
    if !st.entity_active {
        return Ok(());
    }
    set_txpwr_now(bus, st)?;
    bus.flush()?;
    Ok(())
}

// Linux: rtw89_chip_set_txpwr_ctrl (core.h:8755): __rtw8852bx_set_txpwr_ctrl
// (rtw8852b_common.c:1379) for PHY 0: the power reference 0
pub fn set_txpwr_ctrl<B: Bus + ?Sized>(bus: &mut B, st: &mut ChanState) -> Rtw89Result<()> {
    txpwr::set_txpwr_ref(bus, 0, &mut st.events)?;
    bus.flush()?;
    Ok(())
}

// Linux: rtw89_chip_init_txpwr_unit (core.h:8793): __rtw8852bx_init_txpwr_unit
// (rtw8852b_common.c:1407) for PHY 0; called inside rtw89_phy_init_bb_reg (phy.c:1903)
pub fn init_txpwr_unit<B: Bus + ?Sized>(bus: &mut B, st: &mut ChanState) -> Rtw89Result<()> {
    txpwr::init_txpwr_unit_chip(bus, &mut st.events)?;
    bus.flush()?;
    Ok(())
}

// Linux: rtw89_chip_power_trim (core.h:8767): __rtw8852bx_power_trim (rtw8852b_common.c:445) =
// rtw8852bx_thermal_trim + rtw8852bx_pa_bias_trim, with phy's rtwdev->pwr_trim (the phycap
// map: only programmed values are written)
pub fn power_trim<B: Bus + ?Sized>(bus: &mut B, phy: &Phy) -> Rtw89Result<()> {
    let t = &phy
        .efuse
        .as_ref()
        .ok_or(Rtw89Error::Invalid {
            what: "chan: power trim before the efuse",
            value: 0,
        })?
        .pwr_trim;
    if t.pg_thermal_trim {
        for (i, &raw) in t.thermal_trim.iter().enumerate() {
            let v = (u32::from(raw & 0x1) << 3) | (u32::from(raw & 0x1f) >> 1);
            rf_w(bus, i as u32, RR_TM2, RR_TM2_OFF, v)?;
        }
    }
    if t.pg_pa_bias_trim {
        for (i, &raw) in t.pa_bias_trim.iter().enumerate() {
            rf_w(bus, i as u32, RR_BIASA, RR_BIASA_TXG, u32::from(raw & 0xf))?;
            rf_w(bus, i as u32, RR_BIASA, RR_BIASA_TXA, u32::from(raw >> 4))?;
        }
    }
    bus.flush()?;
    Ok(())
}

// Linux: rtw89_chip_cfg_txrx_path (core.h:8860): __rtw8852bx_bb_cfg_txrx_path
// (rtw8852b_common.c:1741) on channel context 0 (zeroed = 2 GHz before the first switch). It is
// the last step of rtw89_phy_dm_init (phy.c:8260), after bb_sethw read the gain bases.
pub fn cfg_txrx_path<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &Phy,
    st: &mut ChanState,
) -> Rtw89Result<()> {
    path::cfg_txrx_path_chip(bus, phy, &st.chan)?;
    bus.flush()?;
    st.dm_init_done = true;
    Ok(())
}
