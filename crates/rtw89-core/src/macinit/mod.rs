//! The MAC start of the RTL8852BE as Linux rtw89 does it in rtw89_core_start
//! (JS js/macinit.mjs with macinit-io/-trx/-imr/-port/-h2c.mjs): the power
//! switch (rtw89_mac_pwr_on/off), rtw89_mac_preinit, rtw89_mac_partial_init,
//! rtw89_mac_init (firmware download, BB/RF enable, system and TRX init, PCIe
//! post-init, offload config) and the MAC part of the start's tail; ports and
//! roles (`port`), the scheduler TX stop/resume (`sch`) and the MAC's own H2Cs
//! (`h2c`). Band 0, station only, dbcc off; every register access in Linux's
//! (= the JS driver's) order.

pub mod h2c;
pub mod imr;
pub mod io;
pub mod port;
pub mod sch;
pub mod trx;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_hostile;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::bringup::power::{self, PowerOn, PwrOpts};
use crate::bus::{Bus, Width};
use crate::error::{Rtw89Error, Rtw89Result};
use crate::events::EventLog;
use crate::fw::{self, Firmware, FwState};
use crate::pci::{self, DmacState, PciRings};
use crate::tables::regs::*;

pub use io::{MacSel, check_mac_en, mac_en_check};
pub use port::{
    FilterFlags, Vif, VifHooks, cfg_ppdu_status, default_rx_fltr, macid_pause, port_update,
    rx_filter, set_rx_fltr, update_rts_threshold, vif_init,
};
pub use sch::{SchTxSel, resume_sch_tx, stop_sch_tx};
pub use trx::{cmac_init, dmac_init, sys_init, trx_init};

/// What Linux only warns or logs about, and the JS driver's `d.log` lines of the MAC
/// part, for the caller's log (B6)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MacEvent {
    /// rtw89_mac_pwr_on: the power-on failed, so off once and on again
    PwrOnRetry { error: Rtw89Error },
    /// a power-off (inside that retry, or after a failed rtw89_mac_init) failed too
    PwrOffFailed { error: Rtw89Error },
    /// rtw89_mac_init failed after the power-on: powered off (`error` is returned too)
    MacInitFailed { error: Rtw89Error },
    /// rst_bacam: "failed to reset BA CAM" (`reg` still read `value`); the init goes on
    BacamResetTimeout { reg: u32, value: u32 },
    /// a wait Linux takes from a value outside the driver was cut to the 10 s rule
    WaitClamped {
        what: &'static str,
        want_ms: u32,
        ms: u32,
    },
    /// rtw89_set_hw_sch_tx_en failed; Linux's callers on this chip go on, so does BitsOS
    SchTxFailed {
        what: &'static str,
        error: Rtw89Error,
    },
}

impl fmt::Display for MacEvent {
    // the JS driver's log lines (macinit*.mjs d.log)
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MacEvent::PwrOnRetry { error } => {
                write!(
                    f,
                    "macinit: power on failed ({error:?}), power off and on again"
                )
            }
            MacEvent::PwrOffFailed { error } => {
                write!(f, "macinit: power off failed too ({error:?})")
            }
            MacEvent::MacInitFailed { error } => {
                write!(f, "macinit: mac init failed ({error:?}), power off")
            }
            MacEvent::BacamResetTimeout { reg, value } => write!(
                f,
                "macinit: BA CAM reset did not finish (0x{reg:x}=0x{value:x})"
            ),
            MacEvent::WaitClamped { what, want_ms, ms } => {
                write!(f, "macinit: {what}: {want_ms} ms cut to {ms} ms")
            }
            MacEvent::SchTxFailed { what, error } => {
                write!(
                    f,
                    "macinit: {what} failed ({error:?}), going on as Linux does"
                )
            }
        }
    }
}

/// B4: the longest waits the MAC part takes from values outside the driver (the
/// firmware's answer to a register command, the AP's beacon interval). Linux waits 1 s
/// and up to 65 s; BitsOS gives each thread its own bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacWaits {
    /// msg_reg's C2H wait of the sch TX command in us (rtw89_set_hw_sch_tx_en; Linux:
    /// RTW89_C2H_TIMEOUT, 1 s)
    pub sch_tx_c2h_us: u32,
    /// the longest msleep(beacon_int + 1) of the port update (rtw89_mac_port_cfg_func_sw)
    /// in ms; a longer beacon interval is cut to it with a `WaitClamped` event
    pub func_sw_ms: u32,
}

impl MacWaits {
    /// The probe and bring-up thread (class Background): Linux's 1 s C2H wait and
    /// spec/wlan.md §2.3's 10 s rule. The golden trace runs with these.
    pub const fn probe() -> MacWaits {
        MacWaits {
            sch_tx_c2h_us: RTW89_C2H_TIMEOUT,
            func_sw_ms: port::FUNC_SW_MAX_WAIT_MS,
        }
    }
    /// Every other caller (KernelRt/Interactive: channel switch, roam, reconnect): the
    /// firmware answers a register command within microseconds, 100 ms is generous; a
    /// beacon interval up to 1000 TU still gets its whole wait. The default.
    pub const fn short() -> MacWaits {
        MacWaits {
            sch_tx_c2h_us: 100_000,
            func_sw_ms: 1_001,
        }
    }
}

impl Default for MacWaits {
    fn default() -> MacWaits {
        MacWaits::short()
    }
}

/// Linux: struct rtw89_mac_info (core.h:5938) as the JS driver's `d.mac`: the DMA state
/// of the pre-init (quota mode, DLE info, HCI flow control), hal.rx_fltr with the last
/// filter flags, the power state after the last switch, the last sch TX error
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacState {
    pub dmac: DmacState,
    /// hal.rx_fltr: DEFAULT_AX_RX_FLTR from rtw89_core_init (core.c:7020) on
    pub rx_fltr: u32,
    /// the filter flags rtw89_ops_configure_filter saw last (none at first)
    pub fif: FilterFlags,
    /// the MAC's power after the last switch that went through (`None`: unknown, e.g.
    /// after a failed power-on)
    pub power_on: Option<bool>,
    /// the error of the last sch TX stop/resume (`None`: it went through)
    pub sch_tx_err: Option<Rtw89Error>,
    /// B4: the bounds of the waits; [`MacWaits::short`] unless the caller is the probe
    pub waits: MacWaits,
    events: EventLog<MacEvent>,
}

impl Default for MacState {
    fn default() -> MacState {
        MacState::with_waits(MacWaits::default())
    }
}

impl MacState {
    /// With [`MacWaits::short`]: what every thread but the probe uses
    pub fn new() -> MacState {
        MacState::default()
    }
    pub fn with_waits(waits: MacWaits) -> MacState {
        MacState {
            dmac: DmacState::default(),
            rx_fltr: default_rx_fltr(),
            fif: FilterFlags::default(),
            power_on: None,
            sch_tx_err: None,
            waits,
            events: EventLog::new(),
        }
    }
    /// What happened since the last call, oldest first (WL-4 drains this into klog after
    /// every call; past the log's cap the oldest go, see `events_dropped`)
    pub fn take_events(&mut self) -> Vec<MacEvent> {
        self.events.take()
    }
    pub fn events(&self) -> &[MacEvent] {
        self.events.as_slice()
    }
    /// Events lost to the cap so far (never reset)
    pub fn events_dropped(&self) -> u64 {
        self.events.dropped()
    }
    /// How many undrained events are kept (default [`crate::events::DEFAULT_EVENT_CAP`])
    pub fn set_event_cap(&mut self, cap: usize) {
        self.events.set_cap(cap);
    }
    /// The events as the JS driver's log lines
    pub fn event_lines(&self) -> Vec<String> {
        self.events
            .as_slice()
            .iter()
            .map(alloc::string::ToString::to_string)
            .collect()
    }
    pub(crate) fn push(&mut self, e: MacEvent) {
        self.events.push(e);
    }
}

// Linux: rtw89_mac_power_switch (mac.c:1521) on, with what it changes in the driver: the
// power state (RTW89_FLAG_POWERON), and for off RTW89_FLAG_FW_RDY cleared (mac.c:1578).
// rtw89_set_entity_state(PHY_0, false) of an off is chan's (WL-2 chan) once it exists.
fn switch<B: Bus + ?Sized>(
    bus: &mut B,
    on: Option<&PwrOpts>,
    rfe: u8,
    m: &mut MacState,
    fw: &mut FwState,
) -> Rtw89Result<PowerOn> {
    match power::power_switch(bus, on, rfe) {
        Ok(p) => {
            m.power_on = Some(on.is_some());
            if on.is_none() {
                fw.ready = false;
            }
            Ok(p)
        }
        Err(e) => {
            m.power_on = None;
            Err(e)
        }
    }
}

// Linux: rtw89_mac_pwr_on (mac.c:1588): on failure off once and on again. Only failures
// of the chip retry (the JS `chipFailure`); a refused access or a gone bus does not
// power-cycle. After a failed power-on the MAC's state is unknown (`power_on` None), so
// a stop powers off.
pub fn mac_pwr_on<B: Bus + ?Sized>(
    bus: &mut B,
    o: &PwrOpts,
    m: &mut MacState,
    fw: &mut FwState,
) -> Rtw89Result<PowerOn> {
    match switch(bus, Some(o), o.rfe, m, fw) {
        Err(e) if e.is_chip_failure() => {
            m.push(MacEvent::PwrOnRetry { error: e });
            match switch(bus, None, o.rfe, m, fw) {
                Err(e2) if !e2.is_chip_failure() => return Err(e2),
                Err(e2) => m.push(MacEvent::PwrOffFailed { error: e2 }),
                Ok(_) => {}
            }
            switch(bus, Some(o), o.rfe, m, fw)
        }
        r => r,
    }
}

// Linux: rtw89_mac_pwr_off (mac.c:1603); thermal_prot_vlv is never set, so no vcore reset
pub fn mac_pwr_off<B: Bus + ?Sized>(
    bus: &mut B,
    rfe: u8,
    m: &mut MacState,
    fw: &mut FwState,
) -> Rtw89Result<()> {
    switch(bus, None, rfe, m, fw).map(|_| ())
}

// Linux: rtw89_mac_preinit (mac.c:4342): power on; AX has no mac_func_en. `o` holds what
// the parsed efuse decides (rfe, autoload, power K; PwrOpts::probe only at probe).
pub fn mac_preinit<B: Bus + ?Sized>(
    bus: &mut B,
    o: &PwrOpts,
    m: &mut MacState,
    fw: &mut FwState,
) -> Rtw89Result<PowerOn> {
    mac_pwr_on(bus, o, m, fw)
}

// Linux: rtw89_mac_partial_init (mac.c:4307) with include_bb false (bbmcu_nr 0): at probe
// (rtw89_chip_efuse_info_setup) and inside rtw89_mac_init. rtw89_chip_bb_preinit is not
// called (include_bb false, and the 8852B has none); rtw89_mac_fwdl_preconfig is none on AX.
pub fn mac_partial_init<B: Bus + ?Sized>(
    bus: &mut B,
    rings: &mut PciRings,
    m: &mut MacState,
    fws: &mut FwState,
    fw: &Firmware,
) -> Rtw89Result<()> {
    partial_init_to_fwdl(bus, rings, m)?;
    fw::download(bus, rings, fw, fws)
}

// Linux: rtw89_mac_partial_init (mac.c:4307) up to rtw89_fw_download:
// rtw89_mac_ctrl_hci_dma_trx (mac.h:1642), rtw89_mac_dmac_pre_init (mac.c:4258), the HCI's
// mac_pre_init = rtw89_pci_ops_mac_pre_init_ax (pci.c:3070)
pub fn partial_init_to_fwdl<B: Bus + ?Sized>(
    bus: &mut B,
    rings: &mut PciRings,
    m: &mut MacState,
) -> Rtw89Result<()> {
    pci::hci_dma_trx(bus, true)?;
    pci::dmac_pre_init(bus, &mut m.dmac)?;
    pci::pre_init(bus, rings)?;
    Ok(())
}

// Linux: __rtw8852bx_mac_enable_bb_rf (rtw8852b_common.c:1998), not 8852BT: the chip op both
// rtw89_mac_init and rtw89_chip_reset_bb_rf call (phy's reset uses this one too)
pub fn enable_bb_rf<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    bus.set(
        Width::W8,
        R_AX_SYS_FUNC_EN,
        B_AX_FEN_BBRSTB | B_AX_FEN_BB_GLB_RSTN,
    )?;
    bus.mask32(R_AX_SPS_DIG_ON_CTRL0, B_AX_REG_ZCDC_H_MASK, 0x1)?;
    bus.set32(R_AX_WLRF_CTRL, B_AX_AFC_AFEDIG)?;
    bus.clr32(R_AX_WLRF_CTRL, B_AX_AFC_AFEDIG)?;
    bus.set32(R_AX_WLRF_CTRL, B_AX_AFC_AFEDIG)?;
    power::xtal_write(bus, XTAL_SI_WL_RFC_S0, 0xC7, FULL_BIT_MASK)?;
    power::xtal_write(bus, XTAL_SI_WL_RFC_S1, 0xC7, FULL_BIT_MASK)?;
    bus.w8(R_AX_PHYREG_SET, PHYREG_SET_XYN_CYCLE)?;
    Ok(())
}

/// What rtw89_mac_init works on: the rings, the MAC and firmware state, the parsed file
pub struct MacInit<'a> {
    pub rings: &'a mut PciRings,
    pub mac: &'a mut MacState,
    pub fws: &'a mut FwState,
    pub fw: &'a Firmware,
    /// the efuse's power options: the preinit's power-on, and rfe for the power-off
    /// after a failure
    pub opts: &'a PwrOpts,
}

// Linux: rtw89_mac_init (mac.c:4360). `preinit`: run rtw89_mac_preinit first;
// rtw89_core_start puts only bb_preinit (none on 8852B) and rtw89_phy_init_bb_afe (no
// AFE element in rtw8852b_fw-2.bin) between them. rtw89_mac_feat_init: nothing for
// bacam_ver RTW89_BACAM_V0. On any failure after the power-on Linux powers off
// (mac.c:4398), so does this, and returns the first error.
pub fn mac_init<B: Bus + ?Sized>(bus: &mut B, c: MacInit<'_>, preinit: bool) -> Rtw89Result<()> {
    let MacInit {
        rings,
        mac,
        fws,
        fw,
        opts,
    } = c;
    if preinit {
        mac_preinit(bus, opts, mac, fws)?;
    }
    let r = (|| -> Rtw89Result<()> {
        mac_partial_init(bus, rings, mac, fws, fw)?;
        enable_bb_rf(bus)?;
        sys_init(bus)?;
        trx_init(bus, mac)?;
        pci::post_init(bus, rings)?;
        fw::send_all_early_h2c(bus, rings, fws)?;
        fw::h2c_set_ofld_cfg(bus, rings, fws)?;
        bus.flush()?;
        Ok(())
    })();
    if let Err(e) = r {
        mac.push(MacEvent::MacInitFailed { error: e.clone() });
        if let Err(e2) = mac_pwr_off(bus, opts.rfe, mac, fws) {
            mac.push(MacEvent::PwrOffFailed { error: e2 });
        }
        return Err(e);
    }
    Ok(())
}

/// A step another module does on the bus, with the H2C path (btc's radio-on in the core start
/// tail: its coex run can send H2Cs on CH12)
pub type CoreStep<'a, B> = dyn FnMut(&mut B, &mut PciRings, &mut FwState) -> Rtw89Result<()> + 'a;

// Linux: rtw89_core_start (core.c:6699-6721), the part after rtw89_phy_dm_init:
// rtw89_mac_set_edcca_mode_bands and rtw89_mac_cfg_phy_rpt_bands are none on AX;
// rtw89_hci_start (rtw89_pci_ops_start: NAPI and interrupt masks) is the kernel driver's
// (WL-4); rtw89_chip_rfk_init_late is none for the 8852B; no TAS. `track_start` queues
// the periodic track work (track_work in 2 s, core.c:6710); `radio_on` is where Linux calls
// rtw89_btc_ntfy_radio_state(BTC_RFCTRL_WL_ON) (btc's). fw.log.enable is only set by
// debugfs, so the firmware log H2C goes with false.
pub fn core_start_tail<B: Bus + ?Sized>(
    bus: &mut B,
    rings: &mut PciRings,
    fws: &mut FwState,
    track_start: Option<&mut dyn FnMut()>,
    radio_on: Option<&mut CoreStep<'_, B>>,
) -> Rtw89Result<()> {
    cfg_ppdu_status(bus, true)?; // rtw89_mac_cfg_ppdu_status_bands, dbcc off
    update_rts_threshold(bus)?;
    bus.flush()?;
    if let Some(t) = track_start {
        t();
    }
    if let Some(r) = radio_on {
        r(bus, rings, fws)?;
    }
    fw::h2c_fw_log(bus, rings, fws, false)?;
    fw::h2c_init_ba_cam();
    bus.flush()?;
    Ok(())
}
