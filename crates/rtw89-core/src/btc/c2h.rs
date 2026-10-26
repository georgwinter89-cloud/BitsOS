//! The firmware's coex reports (C2H class BTFC_FW_EVENT, event BTF_EVNT_RPT) as
//! rtw89_btc_c2h_handle reads them for firmware 0.29.29, and the `_get_reg_status` cases built on
//! them (JS js/btc-c2h.mjs). Kept is what later coex runs read: the report map (CTRL), the TDMA and
//! slot sync checks (they force the policy H2C) and the BTG / pre-AGC readbacks of the MREG report
//! (they decide whether `_set_wl_preagc_ctrl` writes the BB table). Statistics, the coex log and
//! the BT reports are not kept. The bytes come from the chip: every read is bounds-checked.

use alloc::vec::Vec;

use crate::bus::Bus;
use crate::error::Rtw89Result;
use crate::phy::io::phy_r32;
use crate::tables::btc::MON_REG;
use crate::tables::regs::*;

use super::h2c::put_slot;
use super::{BtcEvent, BtcState};

/// BTC_RPT_HDR_SIZE (coex.c:1308): u8 type, le16 len
const RPT_HDR: usize = 3;
/// BTC_CHK_HANG_MAX (coex.c:1311)
const CHK_HANG_MAX: u32 = 3;

fn le16(b: &[u8], at: usize) -> Option<usize> {
    let s = b.get(at..at.checked_add(2)?)?;
    Some(usize::from(u16::from_le_bytes([s[0], s[1]])))
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

// pcinfo->req_len of _chk_btc_report for this version: sizeof the report struct (core.h, all
// __packed); STEP is offsetof(step) + trace_step entries. 0: a type this version does not know.
fn req_len(st: &BtcState, typ: u32) -> usize {
    match typ {
        // rtw89_btc_fbtc_rpt_ctrl_v105: 80
        BTC_RPT_TYPE_CTRL => {
            4 + 4 * RTW89_PHY_NUM as usize + 2 * BTC_BCNT_STA_MAX_V105 as usize + 24 + 32
        }
        BTC_RPT_TYPE_TDMA => 4 + 8, // rtw89_btc_fbtc_tdma_v3
        BTC_RPT_TYPE_SLOT => 8 + 8 * CXST_MAX as usize, // rtw89_btc_fbtc_slots: 152
        BTC_RPT_TYPE_CYSTA => 1384, // rtw89_btc_fbtc_cysta_v5
        // rtw89_btc_fbtc_steps_v3, fcxctrl 1
        BTC_RPT_TYPE_STEP => {
            8usize.saturating_add(4usize.saturating_mul(st.ctrl.trace_step as usize))
        }
        BTC_RPT_TYPE_NULLSTA => 60, // rtw89_btc_fbtc_cynullsta_v2
        BTC_RPT_TYPE_MREG => 4 + 4 * CXMREG_MAX_V2 as usize, // rtw89_btc_fbtc_mreg_val_v2: 84
        BTC_RPT_TYPE_GPIO_DBG => 44, // rtw89_btc_fbtc_gpio_dbg_v1
        BTC_RPT_TYPE_BT_VER => 16,  // rtw89_btc_fbtc_btver_v1
        BTC_RPT_TYPE_BT_SCAN => 16, // rtw89_btc_fbtc_btscan_v2
        BTC_RPT_TYPE_BT_AFH => 24,  // rtw89_btc_fbtc_btafh_v2
        BTC_RPT_TYPE_BT_DEVICE => 12, // rtw89_btc_fbtc_btdevinfo
        _ => 0,                     // BTFRE_UNDEF_TYPE
    }
}

// _chk_btc_err (coex.c:1313) for BTC_DCNT_TDMA_NONSYNC / _SLOT_NONSYNC
fn chk_nonsync(st: &mut BtcState, tdma: bool, differs: bool) {
    let dm = &mut st.dm;
    let (cnt, err) = if tdma {
        (&mut dm.cnt_tdma_nonsync, &mut dm.error_tdma)
    } else {
        (&mut dm.cnt_slot_nonsync, &mut dm.error_slot)
    };
    *cnt = if differs { cnt.saturating_add(1) } else { 0 };
    let was = *err;
    *err = *cnt >= CHK_HANG_MAX;
    if *err && !was {
        let count = *cnt;
        let what = if tdma { "TDMA" } else { "slot table" };
        st.push(BtcEvent::NoSync { what, count });
    }
}

/// `_get_reg_status` reads S0 or S1 by the BTG position: btc_set_rfe leaves ant.btg_pos 0 =
/// RF_PATH_A on the 8852B, so S0 (0x4688)
fn pre_agc_addr(st: &BtcState) -> u32 {
    if st.btg_pos == RF_PATH_A {
        R_BTC_BB_PRE_AGC_S0
    } else {
        R_BTC_BB_PRE_AGC_S1
    }
}

fn pre_agc_on(v: u32) -> u32 {
    u32::from(v & B_BTC_BB_PRE_AGC_MASK == B_BTC_BB_PRE_AGC_VAL)
}

// _search_reg_index (coex.c:1207) over the chip's monitor table
fn search_reg_index(num: usize, typ: u32, target: u32) -> Option<usize> {
    MON_REG
        .iter()
        .take(num)
        .position(|m| u32::from(m.kind) == typ && m.offset == target)
}

// The MREG case of _chk_btc_report: _get_reg_status BB_GNT_MUX_MON and BB_PRE_AGC_MON on the
// struct rtw89_btc_fbtc_mreg_val_v2 just received
fn mreg_readback(st: &mut BtcState, c: &[u8]) {
    let num = c.get(1).copied().map_or(0, usize::from);
    let mon = |addr: u32| search_reg_index(num, REG_BB, addr).and_then(|i| le32(c, 4 + 4 * i));
    let gnt = match mon(R_BTC_BB_BTG_RX) {
        None => BTC_BTGCTRL_BB_GNT_NOTFOUND,
        Some(v) => u32::from(v & B_BTC_BB_GNT_MUX == 0),
    };
    let dm = &mut st.dm;
    dm.wl_btg_rx_rb = if dm.wl_btg_rx == BTC_BTGCTRL_BB_GNT_FWCTRL {
        BTC_BTGCTRL_BB_GNT_FWCTRL
    } else {
        gnt
    };
    let agc = mon(pre_agc_addr(st)).map_or(BTC_PREAGC_NOTFOUND, pre_agc_on);
    let dm = &mut st.dm;
    dm.wl_pre_agc_rb = if dm.wl_pre_agc == BTC_PREAGC_BB_FWCTRL {
        BTC_PREAGC_BB_FWCTRL
    } else {
        agc
    };
}

fn slot_bytes(st: &BtcState) -> Vec<u8> {
    let mut now = alloc::vec![0u8; 8 * st.dm.slot_now.len()];
    for (i, s) in st.dm.slot_now.iter().enumerate() {
        put_slot(&mut now[8 * i..], s);
    }
    now
}

// _chk_btc_report (coex.c:1683) for the 0.29.29 layouts; returns the bytes used, 0 to stop the
// walk
fn chk_report(st: &mut BtcState, buf: &[u8], index: usize) -> usize {
    let (Some(&raw), Some(len)) = (buf.get(index), le16(buf, index + 1)) else {
        return 0;
    };
    let mut typ = u32::from(raw);
    // rtw89_btc_fw_rpt_evnt_ver, fwevntrptl 0: no FDDT report, the later types are one lower
    if typ > __BTC_RPT_TYPE_V0_SAME {
        typ = if typ <= __BTC_RPT_TYPE_V0_MAX {
            typ + 1
        } else {
            BTC_RPT_TYPE_MAX
        };
    }
    let start = index + RPT_HDR;
    let Some(c) = buf.get(start..start.saturating_add(len)) else {
        return 0;
    };
    let req = req_len(st, typ);
    if req == 0 || len != req {
        st.push(BtcEvent::ReportRejected {
            typ,
            len: len as u32,
            want: req as u32,
        });
        return 0; // pcinfo->valid = 0
    }
    match typ {
        BTC_RPT_TYPE_CTRL => {
            // v105: struct rtw89_btc_fbtc_rpt_ctrl_info_v5 at 24; the GNT values and BT counters
            // only feed the log
            st.fwinfo.rpt_en_map = le32(c, 24 + 8).unwrap_or(0);
            st.wl.ver_fw_coex = le32(c, 24).unwrap_or(0);
            st.wl.ver_fw = le32(c, 24 + 4).unwrap_or(0);
            st.dm.wl_fw_cx_offload = 0;
        }
        BTC_RPT_TYPE_TDMA => {
            let differs = c.get(4..12) != Some(&st.dm.tdma_now[..]);
            chk_nonsync(st, true, differs);
        }
        BTC_RPT_TYPE_SLOT => {
            let now = slot_bytes(st);
            let differs = c.get(8..8 + now.len()) != Some(&now[..]);
            chk_nonsync(st, false, differs);
        }
        BTC_RPT_TYPE_MREG => mreg_readback(st, c),
        _ => {}
    }
    len + RPT_HDR
}

// Linux: btc_fw_event (coex.c:3581): _parse_btc_report (coex.c:2658). The scoreboard resync after
// it needs scbd_b2w_update, which only the CTRL v11 report sets.
pub fn fw_event(st: &mut BtcState, buf: &[u8]) {
    let mut index = 0usize;
    loop {
        if index + 2 >= st.ver.info_buf as usize {
            break;
        }
        // Linux reads the length first; a header past the end fails its next check
        if index + RPT_HDR > buf.len() {
            break;
        }
        match le16(buf, index + 1) {
            Some(len) if index + len + RPT_HDR <= buf.len() => {}
            _ => break,
        }
        let used = chk_report(st, buf, index);
        if used == 0 {
            break;
        }
        index += used;
    }
}

// Linux: _get_reg_status (coex.c:1220) with BTC_CSTATUS_BB_PRE_AGC: the pre-AGC state read from
// the BB register itself
pub fn read_pre_agc<B: Bus + ?Sized>(bus: &mut B, st: &BtcState) -> Rtw89Result<u32> {
    Ok(pre_agc_on(phy_r32(bus, pre_agc_addr(st))?))
}
