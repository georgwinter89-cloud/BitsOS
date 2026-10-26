//! Efuse parsing, BB and RF init and the PHY tracking of the RTL8852B as Linux rtw89 does
//! it (phy.c, efuse.c, rtw8852b.c, rtw8852b_common.c), for PHY 0 with dbcc off (JS js/phy.mjs
//! with phy-efuse/-tbl/-bbx/-dm/-trk/-env/-dig.mjs). Each function keeps the JS driver's (=
//! Linux's) order of register accesses and names its Linux original.
//!
//! The state is [`Phy`] (JS `d.phy`, `d.st.efuse`, `d.st.hal` in one place). The chip ops
//! other modules own (chan's TX power and paths, rfk1's calibrations) are reached through
//! [`ChipOps`], as Linux reaches them through rtwdev->chip->ops. Linux's warnings become
//! [`PhyEvent`]s the caller drains (`Phy::take_events`) and logs (B6); the crate never logs.

pub mod bbx;
pub mod dig;
pub mod dm;
pub mod efuse;
pub mod env;
pub mod io;
pub mod tbl;
pub mod trk;

#[cfg(any(test, feature = "std"))]
pub mod chipbus;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_hostile;

use alloc::vec::Vec;
use core::fmt;

use crate::bus::{Bus, BusError, field, prep};
use crate::error::{Rtw89Error, Rtw89Result};
use crate::events::EventLog;
use crate::fw::{self, FwState, H2cReg, RfRegInfo};
use crate::pci::PciRings;
use crate::rfk1;
use crate::tables::phy::CH_BASE;
use crate::tables::regs::*;

pub use dig::{ApRssi, ChInfo, UlTbLink};
pub use dm::{Dig, UlTb};
pub use efuse::{Efuse, EfuseMaps};
pub use env::Env;
pub use tbl::{BbGain, PhyTables, RegLoad, RegTable};
pub use trk::{Cfo, Stat};

use io::phy_mask32;

/// enum rtw89_band
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Band {
    B2g,
    B5g,
    B6g,
}

/// The channel the BB is set to (struct rtw89_chan's band, primary channel, bandwidth in MHz)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chan {
    pub band: Band,
    pub ch: u32,
    pub bw: u32,
}

/// rtw89_get_default_chandef (core.c:396): what the chip is set to before the first channel
/// switch, 2.4 GHz channel 1, 20 MHz
pub const DEFAULT_CHAN: Chan = Chan {
    band: Band::B2g,
    ch: 1,
    bw: 20,
};

/// The struct rtw89_hal fields phy fills (JS `d.st.hal`)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hal {
    pub tx_nss: u32,
    pub rx_nss: u32,
    /// enum rtw89_rf_path_bit (RF_B), 0 = both
    pub antenna_tx: u32,
    pub antenna_rx: u32,
    pub tx_path_diversity: bool,
    pub ant_diversity: bool,
    pub no_eht: bool,
    pub support_cckpd: bool,
    pub support_igi: bool,
    pub thermal_prot_th: u32,
    pub thermal_prot_lv: u32,
}

/// What Linux only warns about in the phy part (rtw89_warn/rtw89_debug), for the caller's
/// log (B6)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhyEvent {
    /// rtw89_parse_efuse_map_ax: "failed to check efuse autoload" (`reg` read `value`)
    EfuseAutoload { reg: u32, value: u32 },
    /// rtw89_dump_physical_efuse_map: "efuse dump (dav=0) failed, retrying (n)"
    EfuseDumpRetry { retry: u32, error: Rtw89Error },
    /// rtw89_phy_init_reg: "invalid PHY package" (no headline for rfe/cv; table left out)
    InvalidPhyPackage {
        table: &'static str,
        rfe: u32,
        cv: u32,
    },
    /// rtw89_phy_init_reg: "failed to load CR" (an ELSE without a matched branch)
    LoadCrFailed {
        table: &'static str,
        addr: u32,
        data: u32,
    },
    /// rtw89_phy_cofig_rf_reg_store: "RF parameters exceed size" (the value was dropped)
    RfStoreFull { path: u32, idx: u32 },
    /// rtw89_phy_init_rf_reg: "rf path %d reg h2c config failed" (the H2C upload's `error`;
    /// `None`: more values than its pages hold)
    RfH2cFailed {
        path: u32,
        error: Option<Rtw89Error>,
    },
    /// rtw89_phy_init_bb_afe: "failed to poll AFE" (`addr` & `mask` != `val` after 11 reads)
    AfePollFailed { cat: u32, addr: u32, last: u32 },
    /// rtw89_phy_preinit_rf_nctl_ax: "failed to poll nctl block" (0x8080 never read 4)
    NctlPollFailed { reg: u32, value: u32 },
    /// rtw89_phy_cfo_parse: "mac_id %d is out of range"
    CfoMacIdRange { mac_id: u32 },
    /// rtw89_phy_cfg_bb_rpl_ofst: "bb rpl ofst with unknown bw" (nothing stored)
    BbGainUnknownBw { addr: u32, bw: u32 },
    /// Not Linux (spec/wlan.md §2.3): a wait the firmware file asked for (an AFE delay row) was
    /// longer than 10 s and was cut to it
    WaitClamped {
        what: &'static str,
        want_us: u32,
        us: u32,
    },
}

impl fmt::Display for PhyEvent {
    // the JS driver's log lines (phy*.mjs d.log) where it has one
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PhyEvent::EfuseAutoload { .. } => write!(f, "failed to check efuse autoload"),
            PhyEvent::EfuseDumpRetry { retry, .. } => {
                write!(f, "efuse dump (dav=0) failed, retrying ({retry})")
            }
            PhyEvent::InvalidPhyPackage { rfe, cv, .. } => {
                write!(f, "invalid PHY package: {rfe}/{cv}")
            }
            PhyEvent::LoadCrFailed { addr, data, .. } => {
                write!(f, "failed to load CR {addr:x}/{data:x}")
            }
            PhyEvent::RfStoreFull { path, idx } => {
                write!(f, "RF parameters exceed size. path={path}, idx={idx}")
            }
            PhyEvent::RfH2cFailed { path, .. } => write!(f, "rf path {path} reg h2c config failed"),
            PhyEvent::AfePollFailed { cat, addr, .. } => {
                write!(f, "failed to poll AFE cat={cat} addr=0x{addr:x}")
            }
            PhyEvent::NctlPollFailed { .. } => write!(f, "failed to poll nctl block"),
            PhyEvent::CfoMacIdRange { mac_id } => write!(f, "mac_id {mac_id} is out of range"),
            PhyEvent::BbGainUnknownBw { .. } => write!(f, "bb rpl ofst with unknown bw"),
            PhyEvent::WaitClamped { what, want_us, us } => {
                write!(f, "{what}: wait of {want_us} us cut to {us} us")
            }
        }
    }
}

/// What the table walks of the bring-up applied (JS `d.phy.loads`)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Loads {
    pub bb: Option<RegLoad>,
    pub bb_gain: Option<RegLoad>,
    /// RADIO_A, RADIO_B
    pub rf: Vec<RegLoad>,
    pub nctl: Option<RegLoad>,
}

/// The phy part of struct rtw89_dev (JS `d.phy`, `d.st.efuse`, `d.st.hal`)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Phy {
    /// hal.cv: picks the tables' branch with the efuse's rfe_type
    pub cv: u32,
    /// hal.aid: AX chips keep 0
    pub aid: u16,
    pub efuse: Option<Efuse>,
    pub hal: Hal,
    /// the tables of the firmware file's elements (`load_tables`)
    pub tables: Option<PhyTables>,
    pub bb_gain: BbGain,
    pub loads: Loads,
    pub stat: Stat,
    /// ewma_thermal_read of phystat.avg_thermal per path (the DPK tracking reads it)
    pub avg_thermal: [u32; efuse::RF_PATHS],
    /// rtwdev->is_tssi_mode per path (set by the RF calibration): thermal from the TSSI meter
    pub tssi_mode: [bool; efuse::RF_PATHS],
    pub env: Env,
    pub dig: Dig,
    pub cfo: Cfo,
    /// struct rtw89_edcca_bak's th_old (0 after the init)
    pub edcca_th_old: u32,
    pub ul_tb: UlTb,
    pub ch_info: ChInfo,
    /// rtw89_phy_nctl_poll saw the NCTL engine answer
    pub nctl_ready: bool,
    /// rtw89_phy_edcca_diag's counter
    pub diag_edcca_cnt: u32,
    events: EventLog<PhyEvent>,
}

impl Phy {
    /// For chip cut `cv` (hal.cv)
    pub fn new(cv: u32) -> Phy {
        Phy {
            cv,
            ..Phy::default()
        }
    }
    /// What happened since the last call, oldest first
    pub fn take_events(&mut self) -> Vec<PhyEvent> {
        self.events.take()
    }
    pub fn events(&self) -> &[PhyEvent] {
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
    pub(crate) fn push(&mut self, e: PhyEvent) {
        self.events.push(e);
    }

    /// rtw89_fw_recognize_elements' tables, from the parsed firmware file
    pub fn load_tables(&mut self, fw: &fw::Firmware) {
        self.tables = Some(PhyTables::from_firmware(fw, self.aid));
    }

    // the efuse's rfe_type and hal.cv: the tables are selected by them
    fn rfe_cv(&self) -> Rtw89Result<(u32, u32)> {
        let ef = self.efuse.as_ref().ok_or(Rtw89Error::Invalid {
            what: "phy tables before the efuse (rfe_type picks them)",
            value: 0,
        })?;
        Ok((u32::from(ef.rfe_type), self.cv))
    }
}

/// The rtw8852b_chip_ops entries other modules own (Linux calls them through
/// rtwdev->chip->ops): chan's rtw8852bx_init_txpwr_unit, set_txpwr_ctrl, power_trim,
/// cfg_txrx_path and rfk1's rtw8852b_rfk_init. They see the phy state (efuse, hal) read-only,
/// but for rfk_init, which clears and reads `tssi_mode` and gets the MAC side
/// ([`rfk1::MacLink`]: scheduler TX stop/resume, H2C) when the caller has it. The chan entries
/// are `chan::init_txpwr_unit`, `chan::set_txpwr_ctrl`, `chan::power_trim` and
/// `chan::cfg_txrx_path` on the caller's `chan::ChanState`.
pub trait ChipOps<B: Bus + ?Sized> {
    fn init_txpwr_unit(&mut self, bus: &mut B, phy: &Phy) -> Rtw89Result<()>;
    fn rfk_init(
        &mut self,
        bus: &mut B,
        phy: &mut Phy,
        mac: Option<rfk1::MacLink<'_>>,
    ) -> Rtw89Result<()>;
    fn set_txpwr_ctrl(&mut self, bus: &mut B, phy: &Phy) -> Rtw89Result<()>;
    fn power_trim(&mut self, bus: &mut B, phy: &Phy) -> Rtw89Result<()>;
    fn cfg_txrx_path(&mut self, bus: &mut B, phy: &Phy) -> Rtw89Result<()>;
}

// Linux: rtw89_parse_efuse_map_ax (efuse.c:281) with rtw8852bx_read_efuse,
// rtw89_parse_phycap_map_ax (efuse.c:350) with rtw8852bx_read_phycap, and
// rtw89_core_setup_phycap (core.c:7166), on the dumps in `maps`: the autoload register and the
// phycap bytes are read from the chip when the dumps do not have them (the phycap dump is
// then kept in `maps`). Sets `phy.efuse` and the hal fields; tx/rx nss and the antennas come
// with mac_setup_phycap.
pub fn efuse_parse<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &mut Phy,
    maps: &mut EfuseMaps,
) -> Rtw89Result<()> {
    let mut ef = Efuse::default();
    let ctl = match maps.ctrl {
        Some(c) => c,
        None => bus.r16(R_AX_SYS_WL_EFUSE_CTRL)?,
    };
    if u32::from(ctl) & B_AX_AUTOLOAD_SUS != 0 {
        ef.valid = true;
    } else {
        phy.push(PhyEvent::EfuseAutoload {
            reg: R_AX_SYS_WL_EFUSE_CTRL,
            value: u32::from(ctl),
        });
    }
    let log = efuse::logical_maps(maps)?;
    ef.dav_log = log[efuse::LOG_SIZE..].to_vec();
    efuse::read_efuse_8852bx(&log, &mut ef);
    let pc = match maps.phycap.take() {
        Some(pc) => pc,
        None => {
            let cv = phy.cv;
            let mut retries = Vec::new();
            let r = efuse::phycap_read(bus, cv, &mut |retry, e| retries.push((retry, e.clone())));
            for (retry, error) in retries {
                phy.push(PhyEvent::EfuseDumpRetry { retry, error });
            }
            r?
        }
    };
    efuse::read_phycap_8852bx(&pc, &mut ef);
    maps.phycap = Some(pc);
    phy.efuse = Some(ef);
    let h = &mut phy.hal;
    h.support_cckpd = phy.cv > CHIP_CAV; // 8852B: cv 1 (CBV) has CCK PD
    h.support_igi = false; // only the 8852A up to CBV
    h.thermal_prot_th = 0; // no thermal quirk
    Ok(())
}

/// The firmware's PHY capabilities (the C2H's fields as they came)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PhyCap {
    pub tx_nss: u32,
    pub rx_nss: u32,
    pub tx_ant: u32,
    pub rx_ant: u32,
}

// Linux: rtw89_mac_setup_phycap (mac.c:3322): part 0 only on AX chips, through the register
// H2C GET_FEATURE (rtw89_mac_read_phycap, mac.c:3177; cnv_efuse_state is a no-op on AX).
// Needs the firmware running. `c2h_timeout_us`: 0 = Linux's RTW89_C2H_TIMEOUT (1 s, the probe
// thread only; B4). An answer of another function is an error (Linux fails the probe).
pub fn mac_setup_phycap<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &mut Phy,
    fws: &mut FwState,
    c2h_timeout_us: u32,
) -> Rtw89Result<PhyCap> {
    let h2c = H2cReg {
        id: RTW89_FWCMD_H2CREG_FUNC_GET_FEATURE,
        content_len: 0,
        w: [prep(RTW89_H2CREG_GET_FEATURE_PART_NUM, 0), 0, 0, 0],
    };
    let c2h =
        fw::msg_reg(bus, fws, Some(&h2c), Some(c2h_timeout_us))?.ok_or(Rtw89Error::Invalid {
            what: "phycap: no C2H answer",
            value: 0,
        })?;
    if c2h.id != RTW89_FWCMD_C2HREG_FUNC_PHY_CAP {
        return Err(Rtw89Error::C2hRegAnswer {
            what: "phycap",
            want: RTW89_FWCMD_C2HREG_FUNC_PHY_CAP,
            id: c2h.id,
            reg: R_AX_C2HREG_DATA0,
            value: c2h.w[0],
        });
    }
    let [w0, w1, _, w3] = c2h.w;
    let cap = PhyCap {
        tx_nss: field(w1, RTW89_C2HREG_PHYCAP_W1_TX_NSS),
        rx_nss: field(w0, RTW89_C2HREG_PHYCAP_W0_RX_NSS),
        tx_ant: field(w3, RTW89_C2HREG_PHYCAP_W3_ANT_TX_NUM),
        rx_ant: field(w3, RTW89_C2HREG_PHYCAP_W3_ANT_RX_NUM),
    };
    let h = &mut phy.hal;
    // chip tx_nss, rx_nss: 2
    h.tx_nss = if cap.tx_nss != 0 {
        cap.tx_nss.min(2)
    } else {
        2
    };
    h.rx_nss = if cap.rx_nss != 0 {
        cap.rx_nss.min(2)
    } else {
        2
    };
    if cap.tx_ant == 1 {
        h.antenna_tx = RF_B;
    }
    if cap.rx_ant == 1 {
        h.antenna_rx = RF_B;
    }
    if cap.tx_nss == 1 && cap.tx_ant == 2 && cap.rx_ant == 2 {
        h.antenna_tx = RF_B;
        h.tx_path_diversity = true;
    }
    // 8852B: rf_path_num 2, so no single-antenna diversity branch
    if field(w1, RTW89_C2HREG_PHYCAP_W1_PROT) < RTW89_C2HREG_PHYCAP_W1_PROT_11BE {
        h.no_eht = true;
    }
    Ok(cap)
}

/// rtw89_phy_init_bb_afe's bound on its poll rounds (Linux: 10 + the first read, 1 us apart)
const AFE_POLL_ROUNDS: u32 = 10;
/// struct rtw89_phy_afe_info: six u32
const AFE_ROW: usize = 24;
/// spec/wlan.md §2.3: no wait longer than 10 s, also when a file asks for one
const MAX_WAIT_US: u32 = crate::bus::MAX_WAIT_MS as u32 * 1000;

// Linux: rtw89_phy_init_bb_afe (phy.c:1913); the 8852B firmware file has no AFE_PWR_SEQ
// element, so this does nothing there. Returns the rows walked.
pub fn bb_afe<B: Bus + ?Sized>(bus: &mut B, phy: &mut Phy) -> Rtw89Result<u32> {
    let afe = phy.tables.as_ref().ok_or(Rtw89Error::Invalid {
        what: "phy tables not loaded (parse the firmware first)",
        value: 0,
    })?;
    let Some((size, payload)) = afe.afe.clone() else {
        return Ok(0);
    };
    let n = (size as usize / AFE_ROW).min(payload.len().saturating_sub(8) / AFE_ROW);
    let word =
        |o: usize| u32::from_le_bytes([payload[o], payload[o + 1], payload[o + 2], payload[o + 3]]);
    for i in 0..n {
        let row = 8 + i * AFE_ROW;
        let [action, cat, cls, addr, mask, val] = core::array::from_fn(|k| word(row + 4 * k));
        if cls != RTW89_FW_AFE_CLASS_P0
            && cls != RTW89_FW_AFE_CLASS_P1
            && cls != RTW89_FW_AFE_CLASS_CMN
        {
            continue;
        }
        if action == RTW89_FW_AFE_ACTION_WRITE {
            if cat == RTW89_FW_AFE_CAT_BB || cat == RTW89_FW_AFE_CAT_BB1 {
                phy_mask32(bus, addr, mask, val)?; // dbcc off: BB1 is the same address
            } else if (RTW89_FW_AFE_CAT_MAC..=RTW89_FW_AFE_CAT_AFEDIG1).contains(&cat) {
                bus.mask32(addr, mask, val)?;
            }
        } else if action == RTW89_FW_AFE_ACTION_POLL {
            // CAT_BB is read like MAC too
            let mut last = 0;
            let mut ok = false;
            for _ in 0..=AFE_POLL_ROUNDS {
                last = bus.r32(addr)?;
                ok = field(last, mask) == val;
                if ok {
                    break;
                }
                bus.delay_us(1)?;
            }
            if !ok {
                phy.push(PhyEvent::AfePollFailed { cat, addr, last });
            }
        } else if action == RTW89_FW_AFE_ACTION_DELAY {
            let us = addr.min(MAX_WAIT_US);
            if us != addr {
                phy.push(PhyEvent::WaitClamped {
                    what: "AFE delay",
                    want_us: addr,
                    us,
                });
            }
            bus.delay_us(us)?;
        }
    }
    bus.flush()?;
    Ok(n as u32)
}

// Linux: rtw89_chip_reset_bb_rf (mac.h:1377): disable_bb_rf, enable_bb_rf
pub fn reset_bb_rf<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    bbx::disable_bb_rf(bus)?;
    bbx::enable_bb_rf(bus)?;
    bus.flush()?;
    Ok(())
}

// rtw89_phy_init_reg's walk with its two messages as events; `config` per applied entry
fn walk(
    phy: &mut Phy,
    table: &RegTable,
    name: &'static str,
    config: impl FnMut(u32, u32) -> Rtw89Result<()>,
) -> Rtw89Result<RegLoad> {
    let (rfe, cv) = phy.rfe_cv()?;
    let (load, stop) = tbl::init_reg(table, rfe, cv, config)?;
    match stop {
        Some(tbl::WalkStop::InvalidPackage { rfe, cv }) => {
            phy.push(PhyEvent::InvalidPhyPackage {
                table: name,
                rfe,
                cv,
            });
        }
        Some(tbl::WalkStop::LoadCr { addr, data }) => {
            phy.push(PhyEvent::LoadCrFailed {
                table: name,
                addr,
                data,
            });
        }
        None => {}
    }
    Ok(load)
}

fn missing(phy: &Phy, what: &'static str, path: u32) -> Rtw89Error {
    let (rfe, cv) = phy.rfe_cv().unwrap_or((0, phy.cv));
    Rtw89Error::PhyTable {
        what,
        path,
        rfe,
        cv,
    }
}

// The tables are taken out of `phy` while a walk runs (no copy of the big RADIO tables) and
// always put back
fn with_tables<R>(
    phy: &mut Phy,
    f: impl FnOnce(&mut Phy, &PhyTables) -> Rtw89Result<R>,
) -> Rtw89Result<R> {
    let t = phy.tables.take().ok_or(Rtw89Error::Invalid {
        what: "phy tables not loaded (parse the firmware first)",
        value: 0,
    })?;
    let r = f(phy, &t);
    phy.tables = Some(t);
    r
}

// Linux: rtw89_phy_init_bb_reg (phy.c:1885): the BB_REG table, rtw89_chip_init_txpwr_unit
// (chan's, PHY 0), the BB_GAIN table into `bb_gain`, rtw89_phy_bb_reset (rtw8852b_bb_reset);
// rtw89_chip_bb_postinit: no op on the 8852B. Tables only from the firmware's elements: a
// missing one stops (Linux would use rtw8852b_table.c).
pub fn init_bb_reg<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &mut Phy,
    ops: &mut dyn ChipOps<B>,
) -> Rtw89Result<()> {
    with_tables(phy, |phy, t| {
        let bb =
            t.bb.as_ref()
                .ok_or_else(|| missing(phy, "no BB_REG element", 0))?;
        let load = walk(phy, bb, "BB_REG", |a, d| tbl::config_bb_reg(bus, a, d))?;
        phy.loads.bb = Some(load);
        Ok(())
    })?;
    bus.flush()?; // rtw89_io_unpack
    ops.init_txpwr_unit(bus, phy)?;
    with_tables(phy, |phy, t| {
        let gain = t
            .bb_gain
            .as_ref()
            .ok_or_else(|| missing(phy, "no BB_GAIN element", 0))?;
        let mut g = core::mem::take(&mut phy.bb_gain);
        let mut unknown = Vec::new();
        let r = walk(phy, gain, "BB_GAIN", |a, d| {
            if let Some(u) = tbl::config_bb_gain_ax(&mut g, a, d) {
                unknown.push(u);
            }
            Ok(())
        });
        phy.bb_gain = g;
        for u in unknown {
            phy.push(PhyEvent::BbGainUnknownBw {
                addr: u.addr,
                bw: u.bw,
            });
        }
        phy.loads.bb_gain = Some(r?);
        Ok(())
    })?;
    bbx::bb_reset(bus)?;
    bus.flush()?;
    Ok(())
}

// rtw89_phy_config_rf_reg_fw (phy.c:1643) is fw::rf_reg_upload. Linux only warns when it
// fails (the H2C not taken: the CH12 index poll times out); a lost or refused bus is returned.
fn config_rf_reg_fw<B: Bus + ?Sized>(
    bus: &mut B,
    rings: &mut PciRings,
    fws: &mut FwState,
    info: &mut RfRegInfo,
) -> Rtw89Result<Result<bool, Rtw89Error>> {
    match fw::rf_reg_upload(bus, rings, fws, info).and_then(|ok| {
        bus.flush()?;
        Ok(ok)
    }) {
        Ok(ok) => Ok(Ok(ok)),
        Err(
            e @ Rtw89Error::Bus(BusError::Gone | BusError::Transport | BusError::Refused { .. }),
        ) => Err(e),
        Err(e) => Ok(Err(e)),
    }
}

// Linux: rtw89_phy_init_rf_reg (phy.c:2005), noio false: per path the RADIO table through
// rtw89_phy_config_rf_reg_v1 (by_acv false: not 8922D), then its H2C upload. The 8852B has
// rf_path_num 2.
pub fn init_rf_reg<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &mut Phy,
    rings: &mut PciRings,
    fws: &mut FwState,
) -> Rtw89Result<()> {
    phy.loads.rf.clear();
    for path in [RF_PATH_A, RF_PATH_B] {
        let (mut load, mut info) = with_tables(phy, |phy, t| {
            let table =
                t.rf.get(path as usize)
                    .and_then(Option::as_ref)
                    .ok_or_else(|| missing(phy, "no RADIO element", path))?;
            // rf_reg_info->rf_path = rf_table->rf_path
            let mut info = RfRegInfo::new(table.rf_path);
            let mut full = Vec::new();
            let r = walk(phy, table, "RADIO", |a, d| {
                if !tbl::config_rf_reg_v1(bus, a, d, table.rf_path, &mut info)? {
                    full.push(info.curr_idx);
                }
                Ok(())
            });
            for idx in full {
                phy.push(PhyEvent::RfStoreFull {
                    path: table.rf_path,
                    idx,
                });
            }
            Ok((r?, info))
        })?;
        bus.flush()?; // rtw89_io_unpack
        load.h2c_values = info.curr_idx;
        let rf_path = info.rf_path;
        match config_rf_reg_fw(bus, rings, fws, &mut info)? {
            Ok(true) => load.h2c_ok = true,
            r => {
                load.h2c_ok = false;
                phy.push(PhyEvent::RfH2cFailed {
                    path: rf_path,
                    error: r.err(),
                });
            }
        }
        phy.loads.rf.push(load);
    }
    Ok(())
}

// Linux: rtw89_phy_init_rf_nctl (phy.c:2065): pre-init, the NCTL table through
// rtw89_phy_config_bb_reg; the 8852B has no nctl_post_table. Not Linux: a table the walk left
// out stops here (the calibrations would run on an unconfigured NCTL engine; Linux goes on).
pub fn init_rf_nctl<B: Bus + ?Sized>(bus: &mut B, phy: &mut Phy) -> Rtw89Result<()> {
    let timeout = dm::preinit_rf_nctl(bus)?;
    phy.nctl_ready = timeout.is_none();
    if let Some(value) = timeout {
        phy.push(PhyEvent::NctlPollFailed {
            reg: io::BB + dm::NCTL_POLL_REG,
            value,
        });
    }
    let aborted = with_tables(phy, |phy, t| {
        let nctl = t
            .nctl
            .as_ref()
            .ok_or_else(|| missing(phy, "no RF_NCTL element", 0))?;
        let load = walk(phy, nctl, "RF_NCTL", |a, d| tbl::config_bb_reg(bus, a, d))?;
        let aborted = load.aborted;
        phy.loads.nctl = Some(load);
        Ok(aborted)
    })?;
    bus.flush()?;
    if aborted {
        return Err(missing(phy, "NCTL table not loaded", 0));
    }
    Ok(())
}

// Linux: rtw89_phy_dm_init (phy.c:8235), BB 0, in Linux's order; parts that only reset driver
// statistics (diag, nhm without support_noise) write nothing. `chan`: the channel the BB is
// on (None: rtw89_get_default_chandef's); `total_sta_assoc`: rtwdev->total_sta_assoc; `mac`: the
// MAC side rfk1's rfk_init needs (handed to the chip op).
// rtw89_mac_setup_phycap must have run (cfg_txrx_path needs its rx_nss; the probe order
// guarantees it in Linux).
pub fn phy_dm_init<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &mut Phy,
    chan: Option<Chan>,
    total_sta_assoc: u32,
    mac: Option<rfk1::MacLink<'_>>,
    ops: &mut dyn ChipOps<B>,
) -> Rtw89Result<()> {
    let ch = chan.unwrap_or(DEFAULT_CHAN);
    if phy.hal.rx_nss == 0 {
        return Err(Rtw89Error::Invalid {
            what: "phy dm init before the PHY capabilities (hal rx_nss is 0)",
            value: 0,
        });
    }
    let xtal_cap = phy
        .efuse
        .as_ref()
        .ok_or(Rtw89Error::Invalid {
            what: "phy dm init before the efuse",
            value: 0,
        })?
        .xtal_cap;
    // rtw89_phy_stat_init (phy.c:5895): ewma_thermal_init and the first thermal update; the
    // per-BB packet/RSSI statistics feed only debugfs
    phy.stat = Stat::default();
    phy.avg_thermal = [0; efuse::RF_PATHS];
    trk::stat_thermal_update(bus, phy)?;
    phy.hal.thermal_prot_lv = 0;
    if let Some(ef) = phy.efuse.as_mut() {
        bbx::bb_sethw(bus, &mut ef.gain)?; // rtw89_chip_bb_sethw
    }
    dm::env_monitor_init(bus, &mut phy.env)?;
    dm::physts_parsing_init(bus)?;
    dm::dig_init(bus, &mut phy.dig, &phy.hal, &ch, total_sta_assoc > 0)?;
    trk::cfo_init(bus, &mut phy.cfo, xtal_cap)?;
    dm::edcca_init(bus)?; // bb_wrap_init, ch_info_init: none on AX
    phy.edcca_th_old = 0;
    phy.ul_tb = dm::ul_tb_info_init(bus)?;
    // antdiv_init / antdiv_set_ant: no antenna diversity with 2 paths; rfe_gpio, rfk_hw_init:
    // no 8852B op
    init_rf_nctl(bus, phy)?;
    ops.rfk_init(bus, phy, mac)?; // rtw89_chip_rfk_init = rtw8852b_rfk_init
    ops.set_txpwr_ctrl(bus, phy)?; // rtw89_chip_set_txpwr_ctrl, PHY 0
    ops.power_trim(bus, phy)?; // rtw89_chip_power_trim
    ops.cfg_txrx_path(bus, phy)?; // rtw89_chip_cfg_txrx_path
    bus.flush()?;
    Ok(())
}

// Not Linux (JS bringup-steps.mjs checkTables): rtw89_phy_init_reg only logs "invalid PHY
// package" and goes on with the table left out; BitsOS stops before calibrations and
// transmissions run on an unconfigured BB or RF.
pub fn check_tables(phy: &Phy) -> Rtw89Result<()> {
    let bad = |l: &Option<RegLoad>| l.as_ref().is_none_or(|l| l.aborted);
    let (rfe, cv) = phy.rfe_cv().unwrap_or((0, phy.cv));
    let fail = |what, path| {
        Err(Rtw89Error::PhyTable {
            what,
            path,
            rfe,
            cv,
        })
    };
    if bad(&phy.loads.bb) {
        return fail("BB table not loaded", 0);
    }
    if bad(&phy.loads.bb_gain) {
        return fail("BB gain table not loaded", 0);
    }
    for path in 0..efuse::RF_PATHS {
        if phy.loads.rf.get(path).is_none_or(|l| l.aborted) {
            return fail("RADIO table not loaded", path as u32);
        }
    }
    Ok(())
}

// ---- RX: RSSI of a PHY status (PPDU) ----

/// struct rtw89_rx_phy_ppdu, the fields the 8852B's RSSI uses
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Ppdu {
    pub rssi_avg: u8,
    pub rpl_avg: u8,
    pub rssi: [u8; 2],
    pub chan_idx: u8,
    pub valid: bool,
}

/// What rtw89_chip_query_ppdu puts into the RX status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RxStatus {
    /// dBm
    pub signal: i32,
    pub chain_signal: [i32; 2],
    pub chains: u32,
    /// the channel the PPDU came on (`None`: no channel index in it)
    pub band: Option<Band>,
    pub ch: Option<u32>,
    pub freq: Option<u32>,
}

// RTW89_RSSI_RAW_TO_DBM
fn raw_to_dbm(raw: u8) -> i32 {
    i32::from((raw >> RSSI_FACTOR) as i8) - MAX_RSSI as i32
}

// Linux: __rtw8852bx_convert_rpl_to_rssi (rtw8852b_common.c:1985), u8 arithmetic
pub fn convert_rpl_to_rssi(ppdu: &mut Ppdu) {
    let delta = ppdu.rpl_avg.wrapping_sub(ppdu.rssi_avg);
    for r in ppdu.rssi.iter_mut() {
        *r = r.wrapping_add(delta);
    }
    ppdu.rssi_avg = ppdu.rpl_avg;
}

// rtw89_decode_chan_idx (phy.c:8622) and ieee80211_channel_to_frequency: (band, ch, freq)
pub fn decode_chan_idx(idx: u8) -> (Band, u32, u32) {
    let (base, ofs) = (usize::from(idx >> 4), u32::from(idx & 0xf));
    if base == 0 {
        let freq = if ofs == 14 { 2484 } else { 2407 + ofs * 5 };
        return (Band::B2g, ofs, freq);
    }
    let ch = u32::from(CH_BASE.get(base).copied().unwrap_or(0xff)) + (ofs << 1);
    if base <= 5 {
        let freq = if (182..=196).contains(&ch) {
            4000 + ch * 5
        } else {
            5000 + ch * 5
        };
        return (Band::B5g, ch, freq);
    }
    let freq = if ch == 2 { 5935 } else { 5950 + ch * 5 };
    (Band::B6g, ch, freq)
}

// Linux: __rtw8852bx_query_ppdu (rtw8852b_common.c:1967) with rtw8852bx_fill_freq_with_ppdu
pub fn query_ppdu(ppdu: &Ppdu) -> RxStatus {
    let mut s = RxStatus {
        signal: raw_to_dbm(ppdu.rssi[0].max(ppdu.rssi[1])),
        ..RxStatus::default()
    };
    for path in 0..2 {
        s.chains |= 1 << path;
        s.chain_signal[path] = raw_to_dbm(ppdu.rssi[path]);
    }
    if ppdu.valid && ppdu.chan_idx != 0 {
        let (band, ch, freq) = decode_chan_idx(ppdu.chan_idx);
        s.band = Some(band);
        s.ch = Some(ch);
        s.freq = Some(freq);
    }
    s
}

// Linux: rtw89_chip_convert_rpl_to_rssi (core.h:8832), then rtw89_chip_query_ppdu
// (core.h:8822), in the order rtw89_core_parse_phy_status / rx_pending_skb call them
pub fn rx_status(ppdu: &mut Ppdu) -> RxStatus {
    convert_rpl_to_rssi(ppdu);
    query_ppdu(ppdu)
}
