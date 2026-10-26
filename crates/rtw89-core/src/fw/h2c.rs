//! Host-to-chip commands (H2C) of the RTL8852B firmware as Linux rtw89 fw.c
//! builds them (JS js/fw-h2c.mjs): the 8-byte H2C header, sending on CH12,
//! the start-up H2Cs, the RF register upload H2C and the register path
//! (H2CREG/C2HREG).

use alloc::vec;
use alloc::vec::Vec;

use crate::bus::{Bus, BusError, Width, field, prep};
use crate::error::{Rtw89Error, Rtw89Result};
use crate::pci::{self, PciRings};
use crate::tables::regs::*;

use super::download::CH12_WAIT_MS;
use super::{FwEvent, FwState};

/// fw.c:5322 the offload configuration rtw89_fw_h2c_set_ofld_cfg sends
const OFLD_CFG: [u8; 8] = [0x09, 0x00, 0x00, 0x00, 0x5e, 0x00, 0x00, 0x00];
/// fw.c:2827 H2C_LOG_CFG_LEN
const H2C_LOG_CFG_LEN: usize = 12;
/// RTW89_FW_LOG_COMP_* that rtw89_fw_h2c_fw_log turns on when enabled (fw.c:2836)
const LOG_COMPS: [u32; 6] = [
    RTW89_FW_LOG_COMP_INIT,
    RTW89_FW_LOG_COMP_TASK,
    RTW89_FW_LOG_COMP_PS,
    RTW89_FW_LOG_COMP_ERROR,
    RTW89_FW_LOG_COMP_MLO,
    RTW89_FW_LOG_COMP_SCAN,
];
/// 8852B h2c_regs / c2h_regs (rtw8852b.c:124-132): four registers each, the hardware's
const H2C_REGS: [u32; 4] = [
    R_AX_H2CREG_DATA0,
    R_AX_H2CREG_DATA1,
    R_AX_H2CREG_DATA2,
    R_AX_H2CREG_DATA3,
];
const C2H_REGS: [u32; 4] = [
    R_AX_C2HREG_DATA0,
    R_AX_C2HREG_DATA1,
    R_AX_C2HREG_DATA2,
    R_AX_C2HREG_DATA3,
];

/// Category, class and function of an H2C, and the acks it asks for
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct H2c {
    pub cat: u32,
    pub cls: u32,
    pub func: u32,
    pub rack: bool,
    pub dack: bool,
}

fn hdr_words(w0: u32, w1: u32) -> [u8; 8] {
    let mut b = [0u8; 8];
    b[..4].copy_from_slice(&w0.to_le_bytes());
    b[4..].copy_from_slice(&w1.to_le_bytes());
    b
}

// Linux: rtw89_h2c_pkt_set_hdr (fw.c:1632): AX asks for a receive ack on every H2C whose
// sequence number is a multiple of 4; the u8 sequence moves on (and wraps)
pub fn h2c_hdr(st: &mut FwState, typ: u32, h: &H2c, len: u32) -> [u8; 8] {
    let rack = h.rack || st.h2c_seq.is_multiple_of(4);
    let w0 = prep(H2C_HDR_DEL_TYPE, typ)
        | prep(H2C_HDR_CAT, h.cat)
        | prep(H2C_HDR_CLASS, h.cls)
        | prep(H2C_HDR_FUNC, h.func)
        | prep(H2C_HDR_H2C_SEQ, u32::from(st.h2c_seq));
    let w1 = prep(H2C_HDR_TOTAL_LEN, len.wrapping_add(H2C_HEADER_LEN))
        | if rack { H2C_HDR_REC_ACK } else { 0 }
        | if h.dack { H2C_HDR_DONE_ACK } else { 0 };
    st.h2c_seq = st.h2c_seq.wrapping_add(1);
    hdr_words(w0, w1)
}

// Linux: rtw89_h2c_pkt_set_hdr_fwdl (fw.c:1657): no acks, the sequence is not advanced
pub fn h2c_hdr_fwdl(st: &FwState, typ: u32, cat: u32, cls: u32, func: u32, len: u32) -> [u8; 8] {
    let w0 = prep(H2C_HDR_DEL_TYPE, typ)
        | prep(H2C_HDR_CAT, cat)
        | prep(H2C_HDR_CLASS, cls)
        | prep(H2C_HDR_FUNC, func)
        | prep(H2C_HDR_H2C_SEQ, u32::from(st.h2c_seq));
    let w1 = prep(H2C_HDR_TOTAL_LEN, len.wrapping_add(H2C_HEADER_LEN));
    hdr_words(w0, w1)
}

// Linux: rtw89_h2c_tx (core.c:1373) of an H2C made by rtw89_fw_h2c_alloc_skb_with_hdr +
// rtw89_h2c_pkt_set_hdr. Returns when the chip has fetched it (pci::fwcmd waits for the
// CH12 index), so a CH12 error shows up at the H2C that caused it. Returns the header.
// As in Linux the sequence number has moved even when CH12 refuses the H2C (too long, full).
pub fn h2c_send<B: Bus + ?Sized>(
    bus: &mut B,
    rings: &mut PciRings,
    st: &mut FwState,
    h: &H2c,
    payload: &[u8],
) -> Rtw89Result<[u8; 8]> {
    let len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    let hdr = h2c_hdr(st, FWCMD_TYPE_H2C, h, len);
    let mut buf = Vec::with_capacity(8 + payload.len());
    buf.extend_from_slice(&hdr);
    buf.extend_from_slice(payload);
    let sent = pci::fwcmd(bus, rings, &buf, false, CH12_WAIT_MS);
    wifi::secret::wipe(&mut buf); // an H2C can carry a key (SEC_CAM, R8i)
    sent?;
    bus.flush()?;
    Ok(hdr)
}

// Linux: rtw89_fw_h2c_raw (fw.c:8978): a complete H2C (header included) as given, sent like h2c_send
pub fn h2c_raw<B: Bus + ?Sized>(bus: &mut B, rings: &mut PciRings, buf: &[u8]) -> Rtw89Result<()> {
    pci::fwcmd(bus, rings, buf, false, CH12_WAIT_MS)?;
    bus.flush()?;
    Ok(())
}

// Linux: rtw89_fw_send_all_early_h2c (fw.c:9003); the list is filled only by debugfs
pub fn send_all_early_h2c<B: Bus + ?Sized>(
    bus: &mut B,
    rings: &mut PciRings,
    st: &FwState,
) -> Rtw89Result<()> {
    for h2c in &st.early_h2c {
        h2c_raw(bus, rings, h2c)?;
    }
    Ok(())
}

// Linux: rtw89_fw_h2c_set_ofld_cfg (fw.c:5320)
pub fn h2c_set_ofld_cfg<B: Bus + ?Sized>(
    bus: &mut B,
    rings: &mut PciRings,
    st: &mut FwState,
) -> Rtw89Result<[u8; 8]> {
    let h = H2c {
        cat: H2C_CAT_MAC,
        cls: H2C_CL_MAC_FW_OFLD,
        func: H2C_FUNC_OFLD_CFG,
        rack: false,
        dack: true,
    };
    h2c_send(bus, rings, st, &h, &OFLD_CFG)
}

// Linux: rtw89_fw_h2c_fw_log (fw.c:2828); BitsOS sends it with enable false
// (rtwdev->fw.log.enable is only set by debugfs)
pub fn h2c_fw_log<B: Bus + ?Sized>(
    bus: &mut B,
    rings: &mut PciRings,
    st: &mut FwState,
    enable: bool,
) -> Rtw89Result<[u8; 8]> {
    let comp = if enable {
        LOG_COMPS
            .iter()
            .fold(0u32, |m, &c| m | 1u32.wrapping_shl(c))
    } else {
        0
    };
    let mut p = [0u8; H2C_LOG_CFG_LEN];
    // SET_LOG_CFG_LEVEL GENMASK(7, 0), _PATH GENMASK(15, 8) of word 0, _COMP word 1,
    // _COMP_EXT word 2 (fw.h:1900-1918)
    let w0 = prep(0xff, RTW89_FW_LOG_LEVEL_LOUD) | prep(0xff00, 1 << RTW89_FW_LOG_LEVEL_C2H);
    p[..4].copy_from_slice(&w0.to_le_bytes());
    p[4..8].copy_from_slice(&comp.to_le_bytes());
    let h = H2c {
        cat: H2C_CAT_MAC,
        cls: H2C_CL_FW_INFO,
        func: H2C_FUNC_LOG_CFG,
        rack: false,
        dack: false,
    };
    h2c_send(bus, rings, st, &h, &p)
}

// Linux: rtw89_fw_h2c_init_ba_cam (fw.h:5678): 8852B has bacam_ver RTW89_BACAM_V0, so Linux
// sends nothing (only V0_EXT chips init dynamic BA CAM entries)
pub fn h2c_init_ba_cam() {}

/// Linux: struct rtw89_fw_h2c_rf_reg_info (fw.h:4665), zeroed as kzalloc: the RF values
/// rtw89_phy_config_rf_reg stores for the firmware, in RTW89_H2C_RF_PAGE_NUM pages of
/// RTW89_H2C_RF_PAGE_SIZE words (the H2C's format, fixed by the firmware)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RfRegInfo {
    pub rf_path: u32,
    /// rtw89_phy_config_rf_h2c
    pub h2c: Vec<Vec<u32>>,
    pub curr_idx: u32,
}

impl RfRegInfo {
    pub fn new(rf_path: u32) -> RfRegInfo {
        RfRegInfo {
            rf_path,
            h2c: vec![vec![0; RTW89_H2C_RF_PAGE_SIZE as usize]; RTW89_H2C_RF_PAGE_NUM as usize],
            curr_idx: 0,
        }
    }

    // Linux: rtw89_phy_cofig_rf_reg_store (phy.c:1623), for RF writes with addr >= 0x100.
    // false: Linux's "RF parameters exceed size" warning (the value is dropped; the caller logs)
    pub fn store(&mut self, addr: u32, data: u32) -> bool {
        let size = RTW89_H2C_RF_PAGE_SIZE;
        let (idx, page) = (
            (self.curr_idx % size) as usize,
            (self.curr_idx / size) as usize,
        );
        let Some(slot) = self.h2c.get_mut(page).and_then(|p| p.get_mut(idx)) else {
            return false;
        };
        *slot = (addr << 20) | data;
        self.curr_idx += 1;
        true
    }
}

// Linux: rtw89_fw_h2c_rf_reg (fw.c:8090): one page of stored RF values, `len` in bytes
pub fn h2c_rf_reg<B: Bus + ?Sized>(
    bus: &mut B,
    rings: &mut PciRings,
    st: &mut FwState,
    info: &RfRegInfo,
    len: u32,
    page: u32,
) -> Rtw89Result<[u8; 8]> {
    let words = info
        .h2c
        .get(page as usize)
        .and_then(|p| p.get(..(len / 4) as usize))
        .ok_or(Rtw89Error::Invalid {
            what: "RF reg H2C past its page",
            value: len,
        })?;
    let cls = if info.rf_path == RF_PATH_A {
        H2C_CL_OUTSRC_RF_REG_A
    } else {
        H2C_CL_OUTSRC_RF_REG_B
    };
    // __le32 in the C struct; a length that is no multiple of 4 is padded with zeroes as the JS
    let mut payload = vec![0u8; len as usize];
    for (i, w) in words.iter().enumerate() {
        payload[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    let h = H2c {
        cat: H2C_CAT_OUTSRC,
        cls,
        func: page,
        rack: false,
        dack: false,
    };
    h2c_send(bus, rings, st, &h, &payload)
}

// Linux: rtw89_phy_config_rf_reg_fw (phy.c:1643): all stored values as H2C pages, then the
// store emptied (also after an error). Ok(false): more values than the pages hold, nothing
// sent (Linux warns and returns -EINVAL; the JS returns false); an event says so.
pub fn rf_reg_upload<B: Bus + ?Sized>(
    bus: &mut B,
    rings: &mut PciRings,
    st: &mut FwState,
    info: &mut RfRegInfo,
) -> Rtw89Result<bool> {
    let (size, num) = (RTW89_H2C_RF_PAGE_SIZE, RTW89_H2C_RF_PAGE_NUM);
    let mut remain = info.curr_idx;
    let r = (|| {
        if remain > num * size {
            st.push(FwEvent::RfUploadTooLong {
                len: remain,
                max: num * size,
            });
            return Ok(false);
        }
        let mut i = 0;
        while i < num && remain > 0 {
            let len = remain.min(size);
            h2c_rf_reg(bus, rings, st, info, len * 4, i)?;
            remain -= len;
            i += 1;
        }
        Ok(true)
    })();
    info.curr_idx = 0;
    r
}

/// struct rtw89_mac_h2c_info: a command over the H2C registers (`w` = u.h2creg; w0's
/// function and length fields are filled in from `id` and `content_len`)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct H2cReg {
    pub id: u32,
    pub content_len: u32,
    pub w: [u32; 4],
}

/// struct rtw89_mac_c2h_info: the answer over the C2H registers (`w` = u.c2hreg)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct C2hReg {
    pub id: u32,
    /// u8 content_len: (length field << 2) - RTW89_C2HREG_HDR_LEN, wrapping as Linux's
    pub content_len: u8,
    pub w: [u32; 4],
}

// Linux: rtw89_fw_write_h2c_reg (fw.c:9178)
fn write_h2c_reg<B: Bus + ?Sized>(bus: &mut B, st: &mut FwState, info: &H2cReg) -> Rtw89Result<()> {
    // read_poll_timeout(rtw89_read8, val == 0, 1 ms, 5 ms)
    bus.poll(Width::W8, R_AX_H2CREG_CTRL, 0xff, 0, 5)
        .map_err(|e| match e {
            BusError::PollTimeout { addr, last, .. } => Rtw89Error::H2cRegBusy {
                reg: addr,
                value: last,
            },
            e => e.into(),
        })?;
    let len = info
        .content_len
        .saturating_add(RTW89_H2CREG_HDR_LEN)
        .div_ceil(4);
    let (fm, lm) = (RTW89_H2CREG_HDR_FUNC_MASK, RTW89_H2CREG_HDR_LEN_MASK);
    let mut w = info.w;
    w[0] = (w[0] & !fm & !lm) | prep(fm, info.id) | prep(lm, len);
    for (reg, v) in H2C_REGS.iter().zip(w) {
        bus.w32(*reg, v)?;
    }
    st.h2c_counter = st.h2c_counter.wrapping_add(1);
    bus.mask(
        Width::W8,
        R_AX_UDM1 + 1,
        B_AX_UDM1_HALMAC_H2C_DEQ_CNT_MASK >> 8,
        u32::from(st.h2c_counter),
    )?;
    bus.w8(R_AX_H2CREG_CTRL, B_AX_H2CREG_TRIGGER)?;
    Ok(())
}

// Linux: rtw89_fw_read_c2h_reg (fw.c:9211). read_poll_timeout_atomic(val != 0, 1 us,
// timeout): not a mask compare, so a loop of reads with a 1 us delay, bounded by the clock and
// by the waited steps (a clock that does not move cannot make it endless); after the deadline
// one more read decides, as Linux's macro does.
fn read_c2h_reg<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut FwState,
    timeout_us: u32,
) -> Rtw89Result<C2hReg> {
    let timeout_us = if timeout_us == 0 {
        RTW89_C2H_TIMEOUT
    } else {
        timeout_us
    };
    let limit_ms = u64::from(timeout_us.div_ceil(1000));
    let start = bus.now_ms();
    let mut waited_us: u64 = 0;
    loop {
        if bus.r8(R_AX_C2HREG_CTRL)? != 0 {
            break;
        }
        if bus.now_ms().saturating_sub(start) >= limit_ms || waited_us >= u64::from(timeout_us) {
            let v = bus.r8(R_AX_C2HREG_CTRL)?;
            if v != 0 {
                break;
            }
            st.push(FwEvent::C2hRegTimeout {
                reg: R_AX_C2HREG_CTRL,
                value: u32::from(v),
                timeout_us,
            });
            return Err(Rtw89Error::C2hRegTimeout {
                reg: R_AX_C2HREG_CTRL,
                value: u32::from(v),
            });
        }
        bus.delay_us(1)?;
        waited_us += 1;
    }
    let mut w = [0u32; 4];
    for (slot, reg) in w.iter_mut().zip(C2H_REGS) {
        *slot = bus.r32(reg)?;
    }
    bus.w8(R_AX_C2HREG_CTRL, 0)?;
    let out = C2hReg {
        id: field(w[0], RTW89_C2HREG_HDR_FUNC_MASK),
        content_len: ((field(w[0], RTW89_C2HREG_HDR_LEN_MASK) << 2) as u8)
            .wrapping_sub(RTW89_C2HREG_HDR_LEN as u8),
        w,
    };
    st.c2h_counter = st.c2h_counter.wrapping_add(1);
    bus.mask(
        Width::W8,
        R_AX_UDM1 + 1,
        B_AX_UDM1_HALMAC_C2H_ENQ_CNT_MASK >> 8,
        u32::from(st.c2h_counter),
    )?;
    bus.flush()?;
    Ok(out)
}

// Linux: rtw89_fw_msg_reg (fw.c:9256): an H2C over the registers (or none), then the C2H
// answer (or none): `c2h` = Some(timeout in us, 0 for RTW89_C2H_TIMEOUT). Used by
// rtw89_mac_read_phycap (phy) and rtw89_hw_sch_tx_en_h2c (macinit).
// B4: the wait is the caller's: Some(0) (1 s, Linux's default) only from the probe thread;
// KernelRt and Interactive callers pass their own short timeout. A timeout is also an
// FwEvent (Linux's "c2h reg timeout" warning) for the caller's log.
pub fn msg_reg<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut FwState,
    h2c: Option<&H2cReg>,
    c2h: Option<u32>,
) -> Rtw89Result<Option<C2hReg>> {
    if h2c.is_none() && c2h.is_none() {
        return Err(Rtw89Error::Invalid {
            what: "msg_reg: nothing to do",
            value: 0,
        });
    }
    if let Some(h) = h2c {
        write_h2c_reg(bus, st, h)?;
        bus.flush()?;
    }
    match c2h {
        None => Ok(None),
        Some(t) => read_c2h_reg(bus, st, t).map(Some),
    }
}
