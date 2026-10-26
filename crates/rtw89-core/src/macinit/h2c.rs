//! The two firmware commands the MAC part itself sends (JS js/macinit-h2c.mjs):
//! the MACID pause H2C of rtw89_mac_set_macid_pause and the scheduler TX enable
//! through the H2C/C2H registers (fw's `msg_reg`). The start-up H2Cs are fw's,
//! the vif and CAM H2Cs sta's (WL-3), as the JS CONTRACTS.md says.

use crate::bus::{Bus, prep};
use crate::error::{Rtw89Error, Rtw89Result};
use crate::fw::{self, FwState, H2c, H2cReg};
use crate::pci::PciRings;
use crate::tables::regs::*;

/// struct rtw89_fw_macid_pause_grp (fw.h:315): pause_grp[4], mask_grp[4]
pub const MACID_PAUSE_LEN: usize = 32;
/// The groups of 32 MAC IDs the struct has: MAC IDs from 0 to 127
pub const MACID_PAUSE_GRPS: u32 = 4;

// Linux: rtw89_fw_h2c_macid_pause (fw.c:5180), the payload without MACID_PAUSE_SLEEP
// (firmware 0.29.29.18 has no such feature): BIT(sh) in mask_grp[grp], and in
// pause_grp[grp] to pause. A group past the struct is refused (Linux would write past
// its 32 bytes; the 8852B has 128 MAC IDs, so no caller of its gets there).
pub fn macid_pause_payload(sh: u32, grp: u32, pause: bool) -> Rtw89Result<[u8; MACID_PAUSE_LEN]> {
    if sh >= 32 || grp >= MACID_PAUSE_GRPS {
        return Err(Rtw89Error::Invalid {
            what: "macid pause: MAC ID group past the H2C",
            value: grp.saturating_mul(32).saturating_add(sh),
        });
    }
    let mut p = [0u8; MACID_PAUSE_LEN];
    let set = (1u32 << sh).to_le_bytes();
    let g = grp as usize * 4;
    p[16 + g..16 + g + 4].copy_from_slice(&set);
    if pause {
        p[g..g + 4].copy_from_slice(&set);
    }
    Ok(p)
}

// Linux: rtw89_fw_h2c_macid_pause (fw.c:5180): cat MAC, class FW_OFLD, func MACID_PAUSE,
// receive ack asked, no done ack. Linux's caller ignores a failed send (it only logs);
// the JS throws it, and so does this.
pub fn h2c_macid_pause<B: Bus + ?Sized>(
    bus: &mut B,
    rings: &mut PciRings,
    st: &mut FwState,
    sh: u32,
    grp: u32,
    pause: bool,
) -> Rtw89Result<[u8; 8]> {
    let p = macid_pause_payload(sh, grp, pause)?;
    let h = H2c {
        cat: H2C_CAT_MAC,
        cls: H2C_CL_MAC_FW_OFLD,
        func: H2C_FUNC_MAC_MACID_PAUSE,
        rack: true,
        dack: false,
    };
    fw::h2c_send(bus, rings, st, &h, &p)
}

// Linux: rtw89_hw_sch_tx_en_h2c (mac.c:3338) through rtw89_fw_msg_reg (fw.c:9256);
// `c2h_timeout_us` is msg_reg's wait (0 = RTW89_C2H_TIMEOUT, B4: the caller's choice).
// An answer of another function is Linux's -EINVAL, here with the register and its value.
pub fn hw_sch_tx_en_h2c<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut FwState,
    band: u32,
    tx_en: u16,
    mask: u16,
    c2h_timeout_us: u32,
) -> Rtw89Result<()> {
    let w0 = prep(RTW89_H2CREG_SCH_TX_EN_W0_EN, u32::from(tx_en));
    let w1 = prep(RTW89_H2CREG_SCH_TX_EN_W1_MASK, u32::from(mask))
        | prep(RTW89_H2CREG_SCH_TX_EN_W1_BAND, band);
    // content_len: sizeof(struct rtw89_h2creg_sch_tx_en) (w0, w1) - RTW89_H2CREG_HDR_LEN
    let h = H2cReg {
        id: RTW89_FWCMD_H2CREG_FUNC_SCH_TX_EN,
        content_len: 8 - RTW89_H2CREG_HDR_LEN,
        w: [w0, w1, 0, 0],
    };
    let c2h = fw::msg_reg(bus, st, Some(&h), Some(c2h_timeout_us))?;
    match c2h {
        Some(c) if c.id == RTW89_FWCMD_C2HREG_FUNC_TX_PAUSE_RPT => Ok(()),
        c => {
            let c = c.unwrap_or_default();
            Err(Rtw89Error::C2hRegAnswer {
                what: "sch tx en",
                want: RTW89_FWCMD_C2HREG_FUNC_TX_PAUSE_RPT,
                id: c.id,
                reg: R_AX_C2HREG_DATA0,
                value: c.w[0],
            })
        }
    }
}
