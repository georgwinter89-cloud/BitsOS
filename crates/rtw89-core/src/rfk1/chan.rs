//! The RF part of a channel switch (rtw8852b_set_channel_rf: RF 0x18 channel and bandwidth on
//! both paths, the LO lock check, the RX baseband filter) and the RFK channel slot
//! (rtw8852b_mcc_get_ch_info); JS js/rfk1-chan.mjs.

use crate::bus::{Bus, prep};
use crate::error::Rtw89Result;
use crate::phy::Chan;
use crate::phy::io::{RFREG_MASK, rf_r, rf_w};
use crate::tables::regs::*;

use super::util::{
    INV_RF_DATA, RF_PATH_NUM_8852B, RfkChan, chan_get, debug_rf_read, kpath, poll_read,
};
use super::{LckStep, RfkEvent, RfkState};

const RF_PATH_A: u32 = 0;
const RF_PATH_B: u32 = 1;

/// RF 0x18 bits _bw_setting and _ch_setting clear before the write (cut to the 20-bit register)
const fn not_mask(bits: u32) -> u32 {
    !bits & RFREG_MASK
}

// Linux: _bw_setting (rtw8852b_rfk.c:3947)
fn bw_setting<B: Bus + ?Sized>(bus: &mut B, path: u32, bw: u32, dav: bool) -> Rtw89Result<()> {
    let reg18 = if dav { RR_CFGCH } else { RR_CFGCH_V1 };
    let mut v = rf_r(bus, path, reg18, RFREG_MASK)?;
    if v == INV_RF_DATA {
        return Ok(());
    }
    v &= !RR_CFGCH_BW;
    if bw == RTW89_CHANNEL_WIDTH_5 || bw == RTW89_CHANNEL_WIDTH_10 || bw == RTW89_CHANNEL_WIDTH_20 {
        v |= prep(RR_CFGCH_BW, CFGCH_BW_20M);
    } else if bw == RTW89_CHANNEL_WIDTH_40 {
        v |= prep(RR_CFGCH_BW, CFGCH_BW_40M);
    } else if bw == RTW89_CHANNEL_WIDTH_80 {
        v |= prep(RR_CFGCH_BW, CFGCH_BW_80M);
    }
    v &= not_mask(RR_CFGCH_POW_LCK | RR_CFGCH_TRX_AH | RR_CFGCH_BCN | RR_CFGCH_BW2);
    v |= RR_CFGCH_BW2;
    rf_w(bus, path, reg18, RFREG_MASK, v)?;
    debug_rf_read(bus, path, reg18) // debug read: RF 0x18
}

// Linux: _ctrl_bw (rtw8852b_rfk.c:3989)
fn ctrl_bw<B: Bus + ?Sized>(bus: &mut B, bw: u32) -> Rtw89Result<()> {
    bw_setting(bus, RF_PATH_A, bw, true)?;
    bw_setting(bus, RF_PATH_B, bw, true)?;
    bw_setting(bus, RF_PATH_A, bw, false)?;
    bw_setting(bus, RF_PATH_B, bw, false)
}

// Linux: _set_s0_arfc18 (rtw8852b_rfk.c:3998): RF 0x18 of path A with the LDO select set, then
// wait until the synthesizer's LPF is no longer busy. Returns whether that wait timed out.
fn set_s0_arfc18<B: Bus + ?Sized>(bus: &mut B, st: &mut RfkState, val: u32) -> Rtw89Result<bool> {
    let bak = rf_r(bus, RF_PATH_A, RR_LDO, RFREG_MASK)?;
    rf_w(bus, RF_PATH_A, RR_LDO, RR_LDO_SEL, 0x1)?;
    rf_w(bus, RF_PATH_A, RR_CFGCH, RFREG_MASK, val)?;
    let poll = poll_read(
        bus,
        |b| rf_r(b, RF_PATH_A, RR_LPF, RR_LPF_BUSY),
        |v| v == 0,
        1,
        1000,
    )?;
    rf_w(bus, RF_PATH_A, RR_LDO, RFREG_MASK, bak)?;
    if poll.timed_out {
        st.push(RfkEvent::LpfBusyTimeout { value: poll.val });
    }
    Ok(poll.timed_out)
}

fn lo_locked<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<bool> {
    Ok(rf_r(bus, RF_PATH_A, RR_SYNFB, RR_SYNFB_LK)? != 0)
}

// Linux: _lck_check (rtw8852b_rfk.c:4018): up to three ways to get the LO locked
fn lck_check<B: Bus + ?Sized>(bus: &mut B, st: &mut RfkState) -> Rtw89Result<()> {
    st.lck.clear(); // what was tried, Linux only prints it
    if !lo_locked(bus)? {
        st.lck_step(LckStep::MmdReset);
        rf_w(bus, RF_PATH_A, RR_MMD, RR_MMD_RST_EN, 0x1)?;
        rf_w(bus, RF_PATH_A, RR_MMD, RR_MMD_RST_SYN, 0x0)?;
        rf_w(bus, RF_PATH_A, RR_MMD, RR_MMD_RST_SYN, 0x1)?;
        rf_w(bus, RF_PATH_A, RR_MMD, RR_MMD_RST_EN, 0x0)?;
    }
    bus.delay_us(10)?;
    if !lo_locked(bus)? {
        st.lck_step(LckStep::ResetRf18);
        rf_w(bus, RF_PATH_A, RR_LCK_TRG, RR_LCK_TRGSEL, 0x1)?;
        let tmp = rf_r(bus, RF_PATH_A, RR_CFGCH, RFREG_MASK)?;
        set_s0_arfc18(bus, st, tmp)?;
        rf_w(bus, RF_PATH_A, RR_LCK_TRG, RR_LCK_TRGSEL, 0x0)?;
    }
    if !lo_locked(bus)? {
        st.lck_step(LckStep::SynOffOn);
        let tmp = rf_r(bus, RF_PATH_A, RR_POW, RFREG_MASK)?;
        rf_w(bus, RF_PATH_A, RR_POW, RFREG_MASK, tmp)?;
        let tmp = rf_r(bus, RF_PATH_A, RR_SX, RFREG_MASK)?;
        rf_w(bus, RF_PATH_A, RR_SX, RFREG_MASK, tmp)?;
        rf_w(bus, RF_PATH_A, RR_SYNLUT, RR_SYNLUT_MOD, 0x1)?;
        rf_w(bus, RF_PATH_A, RR_POW, RR_POW_SYN, 0x0)?;
        rf_w(bus, RF_PATH_A, RR_POW, RR_POW_SYN, 0x3)?;
        rf_w(bus, RF_PATH_A, RR_SYNLUT, RR_SYNLUT_MOD, 0x0)?;
        rf_w(bus, RF_PATH_A, RR_LCK_TRG, RR_LCK_TRGSEL, 0x1)?;
        let tmp = rf_r(bus, RF_PATH_A, RR_CFGCH, RFREG_MASK)?;
        set_s0_arfc18(bus, st, tmp)?;
        rf_w(bus, RF_PATH_A, RR_LCK_TRG, RR_LCK_TRGSEL, 0x0)?;
        // debug reads: 0xb2, 0xc5 (C leaves the order of the two open; source order here)
        debug_rf_read(bus, RF_PATH_A, RR_VCO)?;
        debug_rf_read(bus, RF_PATH_A, RR_SYNFB)?;
    }
    Ok(())
}

// Linux: _set_ch (rtw8852b_rfk.c:4066)
fn set_ch<B: Bus + ?Sized>(bus: &mut B, st: &mut RfkState, val: u32) -> Rtw89Result<()> {
    let timeout = set_s0_arfc18(bus, st, val)?;
    if !timeout {
        lck_check(bus, st)?;
    }
    Ok(())
}

// Linux: _ch_setting (rtw8852b_rfk.c:4075)
fn ch_setting<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut RfkState,
    path: u32,
    central_ch: u32,
    dav: bool,
) -> Rtw89Result<()> {
    let reg18 = if dav { RR_CFGCH } else { RR_CFGCH_V1 };
    let is_2g_ch = central_ch <= 14;
    let mut v = rf_r(bus, path, reg18, RFREG_MASK)?;
    v &= not_mask(
        RR_CFGCH_BAND1
            | RR_CFGCH_POW_LCK
            | RR_CFGCH_TRX_AH
            | RR_CFGCH_BCN
            | RR_CFGCH_BAND0
            | RR_CFGCH_CH,
    );
    v |= prep(RR_CFGCH_CH, central_ch);
    if !is_2g_ch {
        v |= prep(RR_CFGCH_BAND1, CFGCH_BAND1_5G) | prep(RR_CFGCH_BAND0, CFGCH_BAND0_5G);
    }
    v &= not_mask(RR_CFGCH_POW_LCK | RR_CFGCH_TRX_AH | RR_CFGCH_BCN | RR_CFGCH_BW2);
    v |= RR_CFGCH_BW2;
    if path == RF_PATH_A && dav {
        set_ch(bus, st, v)?;
    } else {
        rf_w(bus, path, reg18, RFREG_MASK, v)?;
    }
    rf_w(bus, path, RR_LCKST, RR_LCKST_BIN, 0)?;
    rf_w(bus, path, RR_LCKST, RR_LCKST_BIN, 1)?;
    debug_rf_read(bus, path, reg18) // debug read: RF 0x18
}

// Linux: _ctrl_ch (rtw8852b_rfk.c:4111)
fn ctrl_ch<B: Bus + ?Sized>(bus: &mut B, st: &mut RfkState, central_ch: u32) -> Rtw89Result<()> {
    ch_setting(bus, st, RF_PATH_A, central_ch, true)?;
    ch_setting(bus, st, RF_PATH_B, central_ch, true)?;
    ch_setting(bus, st, RF_PATH_A, central_ch, false)?;
    ch_setting(bus, st, RF_PATH_B, central_ch, false)
}

// Linux: _set_rxbb_bw (rtw8852b_rfk.c:4119)
fn set_rxbb_bw<B: Bus + ?Sized>(bus: &mut B, bw: u32, path: u32) -> Rtw89Result<()> {
    rf_w(bus, path, RR_LUTWE2, RR_LUTWE2_RTXBW, 0x1)?;
    rf_w(bus, path, RR_LUTWA, RR_LUTWA_M2, 0x12)?;
    let lb = match bw {
        RTW89_CHANNEL_WIDTH_20 => 0x1b,
        RTW89_CHANNEL_WIDTH_40 => 0x13,
        RTW89_CHANNEL_WIDTH_80 => 0xb,
        _ => 0x3,
    };
    rf_w(bus, path, RR_LUTWD0, RR_LUTWD0_LB, lb)?;
    debug_rf_read(bus, path, RR_LUTWD0)?; // debug read: RF 0x3f
    rf_w(bus, path, RR_LUTWE2, RR_LUTWE2_RTXBW, 0x0)
}

// Linux: _rxbb_bw (rtw8852b_rfk.c:4140)
fn rxbb_bw<B: Bus + ?Sized>(bus: &mut B, phy: u32, bw: u32) -> Rtw89Result<()> {
    let kp = kpath(phy);
    for path in 0..RF_PATH_NUM_8852B {
        if kp & (1 << path) == 0 {
            continue;
        }
        set_rxbb_bw(bus, bw, path)?;
    }
    Ok(())
}

// Linux: rtw8852b_set_channel_rf (rtw8852b_rfk.c:4164) with rtw8852b_ctrl_bw_ch (:4155)
pub fn set_channel_rf<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut RfkState,
    chan: Option<&Chan>,
    phy_idx: u32,
) -> Rtw89Result<()> {
    let c = chan_get(chan)?;
    ctrl_ch(bus, st, c.channel)?;
    ctrl_bw(bus, c.band_width)?;
    rxbb_bw(bus, phy_idx, c.band_width)?;
    bus.flush()?;
    Ok(())
}

/// struct rtw89_rfk_chan_desc
struct ChanDesc {
    ch: u32,
    has_band: bool,
    band: u32,
}

// Linux: rfk_chan_is_equivalent (phy.c:8348) with rfk_chan_validate_desc (:8343)
fn chan_is_equivalent(desc: &ChanDesc, chan: &RfkChan) -> bool {
    if desc.ch == 0 {
        return false;
    }
    if desc.ch != chan.channel {
        return false;
    }
    if desc.has_band && desc.band != chan.band_type {
        return false;
    }
    true
}

// Linux: rtw89_rfk_chan_lookup (phy.c:8381); the entity channels BitsOS has
// (rtw89_iterate_entity_chan) are just the one of channel context 0
fn rfk_chan_lookup(desc: &[ChanDesc], target: &RfkChan, entity: &[RfkChan]) -> usize {
    let mut sel = None;
    for (i, d) in desc.iter().enumerate() {
        if chan_is_equivalent(d, target) {
            return i;
        }
        let found = entity.iter().any(|c| chan_is_equivalent(d, c));
        if !found && sel.is_none() {
            sel = Some(i);
        }
    }
    sel.unwrap_or(0)
}

// Linux: rtw8852b_mcc_get_ch_info (rtw8852b_rfk.c:4172): which of the two RFK result slots
// (table_idx) the current channel uses
pub fn mcc_get_ch_info(st: &mut RfkState, chan: Option<&Chan>, _phy_idx: u32) -> Rtw89Result<()> {
    let c = chan_get(chan)?; // rtw89_entity_get_conf: chans[0]
    let mcc = &mut st.mcc;
    let desc = [0, 1].map(|idx| ChanDesc {
        ch: mcc.ch[idx],
        has_band: true,
        band: mcc.band[idx],
    });
    let idx = rfk_chan_lookup(&desc, &c, &[c]);
    mcc.ch[idx] = c.channel;
    mcc.band[idx] = c.band_type;
    mcc.table_idx = idx as u8;
    Ok(())
}
