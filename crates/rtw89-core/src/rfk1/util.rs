//! What all calibrations of rtw8852b_rfk.c share (JS js/rfk1-util.mjs): the channel they run
//! on, the poll loop of read_poll_timeout, the table parser of phy.c, kpath, register backup
//! and restore, the wait for RX mode and `_set_rx_dck`. rfk2 (TSSI, DPK) uses them too.

use alloc::vec::Vec;

use crate::bus::{Bus, prep};
use crate::error::{Rtw89Error, Rtw89Result};
use crate::events::EventLog;
use crate::phy::io::{
    MASKDWORD, RFREG_MASK, phy_clr32, phy_mask32, phy_r32, phy_set32, rf_r, rf_w,
};
use crate::phy::{Band, Chan};
use crate::tables::Rfk;
use crate::tables::regs::*;
use crate::tables::rfk1::{BACKUP_BB_REGS, BACKUP_RF_REGS};

use super::RfkEvent;

/// rtw8852b.h:10 RF_PATH_NUM_8852B: the chip's two RF paths
pub const RF_PATH_NUM_8852B: u32 = 2;
/// kit/phyio.mjs INV_RF_DATA: what Linux's RF read returns when the serial interface stays busy
pub const INV_RF_DATA: u32 = 0xffff_ffff;

/// The struct rtw89_chan fields the calibrations read (rtw89_chan_create, chan.c:131)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RfkChan {
    pub channel: u32,
    /// enum rtw89_band
    pub band_type: u32,
    /// enum rtw89_bandwidth
    pub band_width: u32,
    /// enum rtw89_subband
    pub subband_type: u32,
}

// Linux: rtw89_get_subband_type (chan.c:19), 2 and 5 GHz
pub fn get_subband_type(band: u32, ch: u32) -> u32 {
    if band != RTW89_BAND_5G {
        return RTW89_CH_2G;
    }
    if (100..=144).contains(&ch) {
        return RTW89_CH_5G_BAND_3;
    }
    if (149..=177).contains(&ch) {
        return RTW89_CH_5G_BAND_4;
    }
    RTW89_CH_5G_BAND_1
}

// Linux: rtw89_chan_get (core.h:8601) for the one channel context BitsOS has: `chan` as phy
// keeps it; `None` before the first rtw89_set_channel, where Linux's rtw89_chan is still zero
// (2 GHz, channel 0): rtw89_entity_init sets only the chandef (chan.c:239, :324). 6 GHz and
// bandwidths other than 20/40/80 MHz are refused (no code for them here, as the JS).
pub fn chan_get(chan: Option<&Chan>) -> Rtw89Result<RfkChan> {
    let Some(c) = chan else {
        return Ok(RfkChan {
            channel: 0,
            band_type: RTW89_BAND_2G,
            band_width: RTW89_CHANNEL_WIDTH_20,
            subband_type: RTW89_CH_2G,
        });
    };
    let band_type = match c.band {
        Band::B2g => RTW89_BAND_2G,
        Band::B5g => RTW89_BAND_5G,
        Band::B6g => {
            return Err(Rtw89Error::Invalid {
                what: "rfk: 6 GHz channel not supported",
                value: c.ch,
            });
        }
    };
    let band_width = match c.bw {
        20 => RTW89_CHANNEL_WIDTH_20,
        40 => RTW89_CHANNEL_WIDTH_40,
        80 => RTW89_CHANNEL_WIDTH_80,
        bw => {
            return Err(Rtw89Error::Invalid {
                what: "rfk: channel width (MHz) not supported",
                value: bw,
            });
        }
    };
    Ok(RfkChan {
        channel: c.ch,
        band_type,
        band_width,
        subband_type: get_subband_type(band_type, c.ch),
    })
}

// Linux: rtw89_btc_phymap (coex.h:353): which paths, PHY and band an RFK notification to the
// coexistence covers
pub fn btc_phymap(chan: Option<&Chan>, phy_idx: u32, paths: u32) -> Rtw89Result<u32> {
    let band = chan_get(chan)?.band_type;
    Ok(prep(BTC_RFK_PATH_MAP, paths)
        | prep(BTC_RFK_PHY_MAP, 1u32.wrapping_shl(phy_idx))
        | prep(BTC_RFK_BAND_MAP, band))
}

/// What a poll saw: Linux's -ETIMEDOUT and the last value read
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Polled {
    pub timed_out: bool,
    pub val: u32,
}

// Linux: read_poll_timeout_atomic (include/linux/iopoll.h, used in rtw8852b_rfk.c): read until
// `cond` holds; give up after `timeout_us` (the bus clock in whole ms, at least 1, as the JS
// pollRead), reading once more after the time ran out. Linux only notes the timeout and the
// caller goes on, so it is no error. B4: the waited delays bound it too, so a clock that does
// not move cannot make it endless (with a moving clock the clock decides first).
pub fn poll_read<B, R, C>(
    bus: &mut B,
    mut read: R,
    cond: C,
    delay_us: u32,
    timeout_us: u32,
) -> Rtw89Result<Polled>
where
    B: Bus + ?Sized,
    R: FnMut(&mut B) -> Rtw89Result<u32>,
    C: Fn(u32) -> bool,
{
    let ms = u64::from(timeout_us.div_ceil(1000).max(1));
    let end = bus.now_ms().saturating_add(ms);
    let limit_us = (ms + 1) * 1000;
    let mut waited_us: u64 = 0;
    loop {
        let expired = bus.now_ms() > end || waited_us > limit_us;
        let val = read(bus)?;
        if cond(val) {
            return Ok(Polled {
                timed_out: false,
                val,
            });
        }
        if expired {
            return Ok(Polled {
                timed_out: true,
                val,
            });
        }
        if delay_us != 0 {
            bus.delay_us(delay_us)?;
        }
        waited_us += u64::from(delay_us.max(1));
    }
}

// Linux: rtw89_rfk_parser (phy.c:8453) with _rfk_handler (phy.c:8411-8448)
pub fn rfk_parser<B: Bus + ?Sized>(bus: &mut B, tbl: &[Rfk]) -> Rtw89Result<()> {
    for e in tbl {
        match *e {
            Rfk::Wrf {
                path,
                addr,
                mask,
                data,
            } => rf_w(bus, path.into(), addr, mask, data)?,
            Rfk::Wm { addr, mask, data } => phy_mask32(bus, addr, mask, data)?,
            Rfk::Ws { addr, bits } => phy_set32(bus, addr, bits)?,
            Rfk::Wc { addr, bits } => phy_clr32(bus, addr, bits)?,
            Rfk::Delay { us } => bus.delay_us(us)?,
        }
    }
    Ok(())
}

// Linux: _kpath (rtw8852b_rfk.c:277); 8852B on BitsOS: dbcc off, so both paths
pub fn kpath(_phy_idx: u32) -> u32 {
    RF_AB
}

// The reads below are arguments of rtw89_debug in Linux. C evaluates them whether debug output
// is on or not, so the chip sees them; they stay in Linux's place and their values are dropped
// (JS debugBbRead/debugRfRead). Dropping them would only change the access order, not a
// value; grep "debug read".

// Linux: rtw89_phy_read32_mask (phy.h:783) as a debug read
pub fn debug_bb_read<B: Bus + ?Sized>(bus: &mut B, addr: u32) -> Rtw89Result<()> {
    phy_r32(bus, addr)?;
    Ok(())
}

// Linux: rtw89_read_rf (core.h:8266) -> rtw89_phy_read_rf_v1 (phy.c:984) as a debug read
pub fn debug_rf_read<B: Bus + ?Sized>(bus: &mut B, path: u32, addr: u32) -> Rtw89Result<()> {
    rf_r(bus, path, addr, RFREG_MASK)?;
    Ok(())
}

// Linux: _rfk_backup_bb_reg (rtw8852b_rfk.c:177)
pub fn rfk_backup_bb_reg<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<Vec<u32>> {
    BACKUP_BB_REGS
        .iter()
        .map(|&a| phy_r32(bus, a.into()))
        .collect()
}

// Linux: _rfk_backup_rf_reg (rtw8852b_rfk.c:191)
pub fn rfk_backup_rf_reg<B: Bus + ?Sized>(bus: &mut B, path: u32) -> Rtw89Result<Vec<u32>> {
    BACKUP_RF_REGS
        .iter()
        .map(|&a| rf_r(bus, path, a, RFREG_MASK))
        .collect()
}

/// A backup of another length than the register list would restore only part of the
/// registers; refuse it before the first write instead of reporting a partial restore.
fn backup_len_ok(what: &'static str, vals: &[u32], want: usize) -> Rtw89Result<()> {
    if vals.len() == want {
        Ok(())
    } else {
        Err(Rtw89Error::Invalid {
            what,
            value: u32::try_from(vals.len()).unwrap_or(u32::MAX),
        })
    }
}

// Linux: _rfk_restore_bb_reg (rtw8852b_rfk.c:206)
pub fn rfk_restore_bb_reg<B: Bus + ?Sized>(bus: &mut B, vals: &[u32]) -> Rtw89Result<()> {
    backup_len_ok("rfk: bb backup length", vals, BACKUP_BB_REGS.len())?;
    for (&a, &v) in BACKUP_BB_REGS.iter().zip(vals) {
        phy_mask32(bus, a.into(), MASKDWORD, v)?;
    }
    Ok(())
}

// Linux: _rfk_restore_rf_reg (rtw8852b_rfk.c:220)
pub fn rfk_restore_rf_reg<B: Bus + ?Sized>(
    bus: &mut B,
    vals: &[u32],
    path: u32,
) -> Rtw89Result<()> {
    backup_len_ok("rfk: rf backup length", vals, BACKUP_RF_REGS.len())?;
    for (&a, &v) in BACKUP_RF_REGS.iter().zip(vals) {
        rf_w(bus, path, a, RFREG_MASK, v)?;
    }
    Ok(())
}

// Linux: _rfk_rf_direct_cntrl (rtw8852b_rfk.c:235)
pub fn rfk_rf_direct_cntrl<B: Bus + ?Sized>(
    bus: &mut B,
    path: u32,
    is_bybb: bool,
) -> Rtw89Result<()> {
    rf_w(bus, path, RR_RSV1, RR_RSV1_RST, u32::from(is_bybb))
}

// Linux: _rfk_drf_direct_cntrl (rtw8852b_rfk.c:244)
pub fn rfk_drf_direct_cntrl<B: Bus + ?Sized>(
    bus: &mut B,
    path: u32,
    is_bybb: bool,
) -> Rtw89Result<()> {
    rf_w(bus, path, RR_BBDC, RR_BBDC_SEL, u32::from(is_bybb))
}

// Linux: _set_rx_dck (rtw8852b_rfk.c:295)
pub fn set_rx_dck<B: Bus + ?Sized>(bus: &mut B, _phy: u32, path: u32) -> Rtw89Result<()> {
    rf_w(bus, path, RR_DCK1, RR_DCK1_CLR, 0x0)?;
    rf_w(bus, path, RR_DCK, RR_DCK_LV, 0x0)?;
    rf_w(bus, path, RR_DCK, RR_DCK_LV, 0x1)?;
    bus.delay_us(1000)?; // mdelay(1)
    Ok(())
}

// Linux: _wait_rx_mode (rtw8852b_rfk.c:1569): RF mode 2 (TX) must end first; a path that stays
// in TX is only noted (`events`), as Linux goes on
pub fn wait_rx_mode<B: Bus + ?Sized>(
    bus: &mut B,
    kp: u32,
    log: &mut EventLog<RfkEvent>,
) -> Rtw89Result<()> {
    for path in 0..RF_PATH_MAX {
        if kp & (1 << path) == 0 {
            continue;
        }
        let p = poll_read(
            bus,
            |b| rf_r(b, path, RR_MOD, RR_MOD_MASK),
            |mode| mode != 2,
            2,
            5000,
        )?;
        if p.timed_out {
            log.push(RfkEvent::RxModeTimeout { path, mode: p.val });
        }
    }
    Ok(())
}

// Linux: _tmac_tx_pause (rtw8852b_rfk.c:1587)
pub fn tmac_tx_pause<B: Bus + ?Sized>(
    bus: &mut B,
    band_idx: u32,
    is_pause: bool,
    log: &mut EventLog<RfkEvent>,
) -> Rtw89Result<()> {
    if !is_pause {
        return Ok(());
    }
    wait_rx_mode(bus, kpath(band_idx), log)
}
