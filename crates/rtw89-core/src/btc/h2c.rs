//! The coexistence H2C commands (category OUTSRC, class BTFC_SET) as fw.c and coex.c build them
//! for this firmware's btc version (0.29.29: fcxinit 0, fcxctrl 1, fwlrole 2, fcxslots 1, fcxmreg
//! 2, fcxtdma 3, fcxbtcrpt 105), JS js/btc-h2c.mjs. Each returns the payload it sent.

use alloc::vec;
use alloc::vec::Vec;

use crate::bus::Bus;
use crate::error::Rtw89Result;
use crate::fw::H2c;
use crate::tables::BtcSlot;
use crate::tables::btc::MON_REG;
use crate::tables::regs::*;

use super::{BtcEvent, BtcHooks, BtcState};

/// BTC_H2C_MAXLEN (coex.h:10)
pub const H2C_MAXLEN: usize = 2020;
/// The report types rtw89_btc_fw_en_rpt switches one by one (0 .. RPT_EN_MONITER - 1)
pub const RPT_TYPES: u32 = 14;
/// BTF_SET_SLOT_TABLE_VER, the v1 slot table
const SLOT_TABLE_VER: u8 = 1;
/// struct rtw89_btc_btf_set_report_v1's fver for fcxbtcrpt 105
const REPORT_VER: u8 = 5;

fn h2c_hdr(func: u32, dack: bool) -> H2c {
    H2c {
        cat: H2C_CAT_OUTSRC,
        cls: BTFC_SET,
        func,
        rack: false,
        dack,
    }
}

/// The layout of struct rtw89_btc_fbtc_slot (core.h): le16 dur, le32 cxtbl, le16 cxtype
pub fn put_slot(b: &mut [u8], s: &BtcSlot) {
    b[0..2].copy_from_slice(&s.dur.to_le_bytes());
    b[2..6].copy_from_slice(&s.cxtbl.to_le_bytes());
    b[6..8].copy_from_slice(&s.cxtype.to_le_bytes());
}

fn le32_at(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

// Linux: rtw89_fw_h2c_cxdrv_init (fw.c:5772), struct rtw89_h2c_cxinit (14 bytes)
pub fn h2c_cx_init<B: Bus + ?Sized>(
    bus: &mut B,
    st: &BtcState,
    hooks: &mut dyn BtcHooks<B>,
) -> Rtw89Result<Vec<u8>> {
    let init = &st.dm.init_info;
    let md = &st.md;
    let ant = &md.ant;
    let mut b = vec![0u8; 14];
    b[0] = CXDRVINFO_INIT as u8;
    b[1] = (b.len() - 2) as u8; // len - H2C_LEN_CXDRVHDR
    b[2] = ant.typ as u8;
    b[3] = ant.num as u8;
    b[4] = ant.isolation as u8;
    b[5] = ((ant.single_pos & 1)
        | ((ant.diversity & 1) << 1)
        | ((ant.btg_pos & 3) << 2)
        | ((ant.stream_cnt & 0xf) << 4)) as u8;
    b[6] = md.rfe_type as u8;
    b[7] = md.kt_ver as u8;
    b[8] = ((md.bt_solo & 1)
        | ((md.bt0_pos & 1) << 1)
        | ((md.bt0_sw_type & 1) << 2)
        | ((md.wa_type & 7) << 3)) as u8;
    b[9] = md.kt_ver_adie as u8;
    b[10] = init.wl_guard_ch as u8;
    b[11] = ((init.wl_only & 1)
        | ((init.wl_init_ok & 1) << 1)
        | ((init.dbcc_en & 1) << 2)
        | ((init.cx_other & 1) << 3)
        | ((init.bt_only & 1) << 4)) as u8;
    hooks.h2c(bus, &h2c_hdr(SET_DRV_INFO, false), &b)?;
    Ok(b)
}

// Linux: rtw89_fw_h2c_cxdrv_ctrl (fw.c:6819), H2C_LEN_CXDRVINFO_CTRL = 4 + 2; fcxctrl 1 has no
// trace step (only fcxctrl 0)
pub fn h2c_cx_ctrl<B: Bus + ?Sized>(
    bus: &mut B,
    st: &BtcState,
    hooks: &mut dyn BtcHooks<B>,
) -> Rtw89Result<Vec<u8>> {
    let c = &st.ctrl;
    let mut b = vec![0u8; 6];
    b[0] = CXDRVINFO_CTRL as u8;
    b[1] = 4;
    let w = (c.manual & 1) | ((c.igno_bt & 1) << 1) | (u32::from(c.always_freerun) << 2);
    le32_at(&mut b, 2, w);
    hooks.h2c(bus, &h2c_hdr(SET_DRV_INFO, false), &b)?;
    Ok(b)
}

// Linux: rtw89_fw_h2c_cxdrv_role_v2 (fw.c:6314), length 4 + 8 * max_role_num + 12 + 2. The
// per-port stride is 12 + PORT_DATA_OFFSET (4) = 16 bytes for an 8-byte record, as in Linux:
// port 3 is overwritten by the mrole words and port 4 lies past the end (both unused, BitsOS has
// one port). Kept byte for byte.
pub fn h2c_cx_role<B: Bus + ?Sized>(
    bus: &mut B,
    st: &BtcState,
    hooks: &mut dyn BtcHooks<B>,
) -> Rtw89Result<Vec<u8>> {
    let payload = cx_role_payload(st);
    hooks.h2c(bus, &h2c_hdr(SET_DRV_INFO, false), &payload)?;
    Ok(payload)
}

/// The role H2C's bytes (rtw89_fw_h2c_cxdrv_role_v2's layout)
pub fn cx_role_payload(st: &BtcState) -> Vec<u8> {
    let r = &st.wl.role;
    let ports = RTW89_PORT_NUM as usize;
    let len = 4 + 8 * usize::from(st.ver.max_role_num) + 12 + 2;
    let mut b = vec![0u8; len + 6 + 16 * ports]; // room for what Linux writes past len
    b[0] = CXDRVINFO_ROLE as u8;
    b[1] = (len - 2) as u8;
    b[2] = r.connect_cnt as u8;
    b[3] = r.link_mode as u8;
    b[4..6].copy_from_slice(&((r.role_map & 0xfff) as u16).to_le_bytes()); // ROLE_NONE .. ROLE_NAN
    for (i, rl) in r.rlink.iter().take(ports).enumerate() {
        let o = 6 + 16 * i;
        b[o] = ((rl.connected & 1)
            | ((rl.pid & 7) << 1)
            | ((rl.phy & 1) << 4)
            | ((rl.noa & 1) << 5)
            | ((rl.rf_band & 3) << 6)) as u8;
        b[o + 1] = ((rl.client_ps & 1) | ((rl.bw & 0x7f) << 1)) as u8;
        b[o + 2] = rl.role as u8;
        b[o + 3] = rl.ch as u8;
        le32_at(&mut b, o + 4, rl.noa_dur);
    }
    let off = len - 12; // H2C_LEN_CXDRVINFO_ROLE_DBCC_LEN
    le32_at(&mut b, off, r.mrole_type);
    le32_at(&mut b, off + 4, r.mrole_noa_duration);
    let dbcc = (r.dbcc_en & 1)
        | ((r.dbcc_chg & 1) << 1)
        | ((r.dbcc_2g_phy & 3) << 2)
        | ((r.link_mode_chg & 1) << 4);
    le32_at(&mut b, off + 8, dbcc);
    b.truncate(len);
    b
}

// Linux: _send_fw_cmd (coex.c:1013). 8852B para_ver 0: no H2C macro buffer (io_oflld_type
// NO_SUPPORT), every command goes out at once with done-ack set. Returns whether it was sent; a
// refusal is counted (cnt_h2c_fail) and logged.
pub fn send_fw_cmd<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    hooks: &mut dyn BtcHooks<B>,
    func: u32,
    buf: &[u8],
) -> Rtw89Result<bool> {
    let s = &st.wl.status;
    let rf_off = BTC_LPS_RF_OFF;
    let refused = buf.len() > H2C_MAXLEN
        || buf.is_empty()
        || !s.init_ok
        || (s.rf_off_pre == rf_off && s.rf_off == rf_off)
        || (s.lps_pre == rf_off && s.lps == rf_off);
    if refused {
        st.fwinfo.cnt_h2c_fail = st.fwinfo.cnt_h2c_fail.wrapping_add(1);
        st.push(BtcEvent::H2cRefused {
            func,
            len: buf.len() as u32,
        });
        return Ok(false);
    }
    hooks.h2c(bus, &h2c_hdr(func, true), buf)?;
    st.fwinfo.cnt_h2c = st.fwinfo.cnt_h2c.wrapping_add(1);
    Ok(true)
}

// Linux: rtw89_btc_fw_set_slots (coex.c:3047), fcxslots 1: struct
// rtw89_btc_btf_set_slot_table { fver = BTF_SET_SLOT_TABLE_VER (1), tbl_num, slots }
pub fn fw_set_slots<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    hooks: &mut dyn BtcHooks<B>,
) -> Rtw89Result<Vec<u8>> {
    let slots = &st.dm.slot;
    let mut b = vec![0u8; 2 + 8 * slots.len()];
    b[0] = SLOT_TABLE_VER;
    b[1] = slots.len() as u8;
    for (i, s) in slots.iter().enumerate() {
        put_slot(&mut b[2 + 8 * i..], s);
    }
    send_fw_cmd(bus, st, hooks, SET_SLOT_TABLE, &b)?;
    Ok(b)
}

// Linux: btc_fw_set_monreg (coex.c:3133), fcxmreg 2 uses the v1 layout:
// { fver, reg_num, regs[] { le16 type, le16 bytes, le32 offset } }
pub fn fw_set_monreg<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    hooks: &mut dyn BtcHooks<B>,
) -> Rtw89Result<Option<Vec<u8>>> {
    if MON_REG.len() > CXMREG_MAX_V2 as usize {
        return Ok(None);
    }
    let mut b = vec![0u8; 2 + 8 * MON_REG.len()];
    b[0] = st.ver.fcxmreg;
    b[1] = MON_REG.len() as u8;
    for (i, m) in MON_REG.iter().enumerate() {
        let o = 2 + 8 * i;
        b[o..o + 2].copy_from_slice(&m.kind.to_le_bytes());
        b[o + 2..o + 4].copy_from_slice(&m.bytes.to_le_bytes());
        le32_at(&mut b, o + 4, m.offset);
    }
    send_fw_cmd(bus, st, hooks, SET_MREG_TABLE, &b)?;
    Ok(Some(b))
}

// Linux: rtw89_btc_fw_en_rpt (coex.c:3086), fcxbtcrpt 105: one rtw89_btc_btf_set_report_v1
// { u8 fver = 5, le32 enable, le32 para } per report type (14 commands). The new map holds only
// when the last command went out, as Linux.
pub fn fw_en_rpt<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    hooks: &mut dyn BtcHooks<B>,
    force: bool,
    rpt_map: u32,
) -> Rtw89Result<()> {
    if !force && rpt_map == st.fwinfo.rpt_en_map {
        return Ok(());
    }
    let mut sent = true;
    for i in 0..RPT_TYPES {
        let mut b = [0u8; 9];
        b[0] = REPORT_VER;
        le32_at(&mut b, 1, i);
        le32_at(&mut b, 5, u32::from(rpt_map & (1 << i) != 0));
        sent = send_fw_cmd(bus, st, hooks, SET_REPORT_EN, &b)?;
    }
    if sent {
        st.fwinfo.rpt_en_map = rpt_map;
    }
    Ok(())
}

// Linux: _append_tdma (coex.c:2689) and _append_slot_v1 (coex.c:2737): the CXPOLICY TLVs of a
// SET_CX_POLICY command
pub fn policy_tlvs(st: &BtcState) -> Vec<u8> {
    let dm = &st.dm;
    let mut out = Vec::new();
    if st.update_policy_force || dm.tdma != dm.tdma_now {
        let mut t = [0u8; 2 + 12]; // fcxtdma 3: struct rtw89_btc_fbtc_tdma_v3
        t[0] = 0; // CXPOLICY_TDMA
        t[1] = 12;
        t[2] = st.ver.fcxtdma;
        t[6..14].copy_from_slice(&dm.tdma);
        out.extend_from_slice(&t);
    }
    for (i, s) in dm.slot.iter().enumerate() {
        if !st.update_policy_force && dm.slot_now.get(i) == Some(s) {
            continue;
        }
        let mut t = [0u8; 2 + 10]; // struct btc_fbtc_1slot
        t[0] = 1; // CXPOLICY_SLOT
        t[1] = 10;
        t[2] = st.ver.fcxslots;
        t[3] = i as u8;
        put_slot(&mut t[4..], s);
        out.extend_from_slice(&t);
    }
    out
}
