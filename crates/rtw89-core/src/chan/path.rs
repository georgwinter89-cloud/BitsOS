//! The RX/TX path set-up of the RTL8852B at the end of the PHY init (JS js/chan-path.mjs; Linux
//! rtw8852b_common.c __rtw8852bx_bb_cfg_txrx_path). __rtw8852bx_bb_ctrl_rx_path itself is phy's
//! (`phy::bbx::ctrl_rx_path`, rfk2's TSSI alignment calls it too).

use crate::bus::Bus;
use crate::error::{Rtw89Error, Rtw89Result};
use crate::phy::Phy;
use crate::phy::bbx::{bb_ctrl_rf_mode_rx_path, ctrl_rx_path};
use crate::phy::io::phy_mask32;
use crate::tables::regs::*;

use super::RtwChan;

// Linux: __rtw8852bx_bb_cfg_txrx_path (rtw8852b_common.c:1741) on the channel context's channel:
// hal->antenna_rx (0 = both paths) and rx_nss as rtw89_mac_setup_phycap_part0 (mac.c:3222) left
// them (0 = the chip's 2 until the firmware said)
pub fn cfg_txrx_path_chip<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &Phy,
    chan: &RtwChan,
) -> Rtw89Result<()> {
    let gain = &phy
        .efuse
        .as_ref()
        .ok_or(Rtw89Error::Invalid {
            what: "chan: cfg_txrx_path before the efuse (gain offsets)",
            value: 0,
        })?
        .gain;
    let hal = &phy.hal;
    let rx_path = if hal.antenna_rx != 0 {
        hal.antenna_rx
    } else {
        RF_AB
    };
    let rx_nss = if rx_path != RF_AB {
        1
    } else if hal.rx_nss != 0 {
        hal.rx_nss
    } else {
        2
    };
    ctrl_rx_path(
        bus,
        rx_path,
        chan.band,
        chan.subband_type,
        gain,
        hal.antenna_rx,
    )?;
    bb_ctrl_rf_mode_rx_path(bus, rx_path)?;
    let two = u32::from(rx_nss != 1);
    phy_mask32(bus, R_RXHT_MCS_LIMIT, B_RXHT_MCS_LIMIT, two)?;
    phy_mask32(bus, R_RXVHT_MCS_LIMIT, B_RXVHT_MCS_LIMIT, two)?;
    phy_mask32(bus, R_RXHE, B_RXHE_MAX_NSS, two)?;
    phy_mask32(bus, R_RXHE, B_RXHETB_MAX_NSS, two)?;
    phy_mask32(bus, R_MAC_SEL, B_MAC_SEL_MOD, 0x0)
}
