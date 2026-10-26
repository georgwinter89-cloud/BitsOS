//! The chip ops stage phy calls, for the driver (WL-4): chan's TX power unit, power control, trim
//! and paths, rfk1/rfk2's rfk_init with btc's RFK notifications and macinit's scheduler TX
//! stop/resume, btc's init and radio-on. The same calls, in the same order, as the golden test's
//! hooks (tests/fake_golden.rs `GoldenHooks`), without its bookkeeping, so the driver's stage phy
//! makes the golden trace (checked by tests/fake_golden/drv.rs).

use crate::bringup::{PciRings, PhyStageHooks};
use crate::btc::{self, BtcState, H2cLink};
use crate::bus::Bus;
use crate::chan::{self, ChanState};
use crate::error::Rtw89Result;
use crate::fw::FwState;
use crate::phy::{ChipOps, Phy};
use crate::rfk1::{self, MacLink, RfkState};
use crate::rfk2::{Rfk2Link, Rfk2State};
use crate::sta::io::BtcRfk;

/// The module states stage phy works on, borrowed from the device being brought up
pub struct StageHooks<'a> {
    pub btc: &'a mut BtcState,
    pub rfk: &'a mut RfkState,
    pub rfk2: &'a mut Rfk2State,
    pub chan: &'a mut ChanState,
}

fn no_mac() -> crate::error::Rtw89Error {
    crate::error::Rtw89Error::Invalid {
        what: "rfk_init without the MAC side",
        value: 0,
    }
}

impl<B: Bus + ?Sized> ChipOps<B> for StageHooks<'_> {
    // Linux: rtw8852bx_init_txpwr_unit (chan's)
    fn init_txpwr_unit(&mut self, bus: &mut B, _: &Phy) -> Rtw89Result<()> {
        chan::init_txpwr_unit(bus, self.chan)
    }
    // Linux: rtw8852b_rfk_init (rtw8852b.c) through rtw89_phy_dm_init: rfk1's parts, rfk2's
    // dpk_init through Rfk2Link, btc's RFK notifications and macinit's scheduler TX stop/resume
    fn rfk_init(
        &mut self,
        bus: &mut B,
        phy: &mut Phy,
        mac: Option<MacLink<'_>>,
    ) -> Rtw89Result<()> {
        let mut hooks = BtcRfk {
            btc: Some(&mut *self.btc),
            mac: mac.ok_or_else(no_mac)?,
        };
        let mut link = Rfk2Link {
            st: &mut *self.rfk2,
            hooks: &mut hooks,
            chan: None,
        };
        rfk1::rfk_init(bus, phy, self.rfk, None, &mut link)
    }
    // Linux: rtw8852bx_set_txpwr_ctrl
    fn set_txpwr_ctrl(&mut self, bus: &mut B, _: &Phy) -> Rtw89Result<()> {
        chan::set_txpwr_ctrl(bus, self.chan)
    }
    // Linux: rtw8852b_power_trim
    fn power_trim(&mut self, bus: &mut B, phy: &Phy) -> Rtw89Result<()> {
        chan::power_trim(bus, phy)
    }
    // Linux: rtw8852bx_cfg_txrx_path
    fn cfg_txrx_path(&mut self, bus: &mut B, phy: &Phy) -> Rtw89Result<()> {
        chan::cfg_txrx_path(bus, phy, self.chan)
    }
}

impl<B: Bus + ?Sized> PhyStageHooks<B> for StageHooks<'_> {
    // Linux: rtw89_btc_ntfy_init(BTC_MODE_NORMAL)
    fn btc_init(
        &mut self,
        bus: &mut B,
        rings: &mut PciRings,
        fws: &mut FwState,
        phy: &Phy,
    ) -> Rtw89Result<()> {
        btc::core_start_init(bus, self.btc, rings, fws, phy)
    }
    // Linux: rtw89_btc_ntfy_radio_state(BTC_RFCTRL_WL_ON)
    fn radio_on(
        &mut self,
        bus: &mut B,
        rings: &mut PciRings,
        fws: &mut FwState,
    ) -> Rtw89Result<()> {
        btc::radio_on(bus, self.btc, &mut H2cLink { fw: fws, rings })
    }
}
