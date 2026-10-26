//! The scheduler TX stop and resume of the RTL8852BE MAC (JS
//! js/macinit-port.mjs `stopSchTx`, `resumeSchTx`; Linux mac.c:3338-3494), MAC 0.

use crate::bus::Bus;
use crate::error::Rtw89Result;
use crate::fw::FwState;
use crate::tables::regs::*;

use super::h2c::hw_sch_tx_en_h2c;
use super::io::{MacSel, check_mac_en, chip_reported, rmw16};
use super::{MacEvent, MacState};

/// enum rtw89_sch_tx_sel
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchTxSel {
    All,
    Hiq,
    Mg0,
    Macid,
}

impl SchTxSel {
    /// The enum's value in Linux (RTW89_SCH_TX_SEL_*); `None` for one it does not have
    pub const fn from_linux(v: u32) -> Option<SchTxSel> {
        match v {
            RTW89_SCH_TX_SEL_ALL => Some(SchTxSel::All),
            RTW89_SCH_TX_SEL_HIQ => Some(SchTxSel::Hiq),
            RTW89_SCH_TX_SEL_MG0 => Some(SchTxSel::Mg0),
            RTW89_SCH_TX_SEL_MACID => Some(SchTxSel::Macid),
            _ => None,
        }
    }
    /// The CTN_TXEN bits rtw89_mac_stop_sch_tx clears for it (mac.c:3411-3435)
    pub const fn mask(self) -> u16 {
        (match self {
            SchTxSel::All | SchTxSel::Macid => B_AX_CTN_TXEN_ALL_MASK,
            SchTxSel::Hiq => B_AX_CTN_TXEN_HGQ,
            SchTxSel::Mg0 => B_AX_CTN_TXEN_MGQ,
        }) as u16
    }
}

// Linux: rtw89_set_hw_sch_tx_en (mac.c:3363): by H2C register once the firmware runs,
// else the register itself (read16, the masked bits replaced, write16: one RMW op)
fn set_hw_sch_tx_en<B: Bus + ?Sized>(
    bus: &mut B,
    m: &MacState,
    fw: &mut FwState,
    tx_en: u16,
    mask: u16,
) -> Rtw89Result<()> {
    check_mac_en(bus, MacSel::Cmac)?;
    if fw.ready {
        return hw_sch_tx_en_h2c(bus, fw, 0, tx_en, mask, m.waits.sch_tx_c2h_us);
    }
    rmw16(bus, R_AX_CTN_TXEN, mask.into(), u32::from(tx_en & mask))?;
    bus.flush()?;
    Ok(())
}

// Linux returns rtw89_set_hw_sch_tx_en's error and every caller on this chip
// (rtw8852b_set_channel_help, rtw8852b_rfk.c) ignores it: the error is kept in
// `m.sch_tx_err` and an event, and the caller goes on. A lost bus or a request the
// driver got wrong is still returned.
fn sch_tx_en<B: Bus + ?Sized>(
    bus: &mut B,
    m: &mut MacState,
    fw: &mut FwState,
    tx_en: u16,
    mask: u16,
    what: &'static str,
) -> Rtw89Result<()> {
    m.sch_tx_err = None;
    match set_hw_sch_tx_en(bus, m, fw, tx_en, mask) {
        Err(e) if chip_reported(&e) => {
            m.push(MacEvent::SchTxFailed {
                what,
                error: e.clone(),
            });
            m.sch_tx_err = Some(e);
            Ok(())
        }
        r => r,
    }
}

// Linux: rtw89_mac_stop_sch_tx (mac.c:3403), MAC 0. Returns tx_en for `resume_sch_tx`,
// also when the stop itself failed (Linux reads it first).
pub fn stop_sch_tx<B: Bus + ?Sized>(
    bus: &mut B,
    m: &mut MacState,
    fw: &mut FwState,
    sel: SchTxSel,
) -> Rtw89Result<u16> {
    let tx_en = bus.r16(R_AX_CTN_TXEN)?;
    sch_tx_en(bus, m, fw, 0, sel.mask(), "stop sch tx")?;
    Ok(tx_en)
}

// Linux: rtw89_mac_resume_sch_tx (mac.c:3485), MAC 0
pub fn resume_sch_tx<B: Bus + ?Sized>(
    bus: &mut B,
    m: &mut MacState,
    fw: &mut FwState,
    tx_en: u16,
) -> Rtw89Result<()> {
    sch_tx_en(
        bus,
        m,
        fw,
        tx_en,
        B_AX_CTN_TXEN_ALL_MASK as u16,
        "resume sch tx",
    )
}
