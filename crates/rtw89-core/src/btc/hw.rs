//! Register-level coexistence of the RTL8852B (JS js/btc-hw.mjs): the MAC side (Linux mac.c: LTE
//! indirect port, PTA init, grant (GNT) control, control path, packet-level table, WL->BT
//! scoreboard) and the chip's own coex ops (rtw8852b.c btc_set_rfe, btc_set_wl_txpwr_ctrl;
//! rtw8852b_common.c __rtw8852bx_btc_*).

use crate::bus::{Bus, Width, field, lowbit, prep};
use crate::error::Rtw89Result;
use crate::phy::bbx::ctrl_nbtg_bt_tx;
use crate::phy::io::{RFREG_MASK, rf_w};
use crate::tables::regs::*;

use super::BtcState;
use super::state::Gnt;

/// The command words for R_AX_LTE_CTRL, literal in rtw89_mac_write_lte (mac.c:97) and
/// rtw89_mac_read_lte (mac.c:112)
const LTE_CMD_WRITE: u32 = 0xC00F_0000;
const LTE_CMD_READ: u32 = 0x800F_0000;
/// BIT(5) of R_AX_LTE_CTRL + 3 (mac.c:91)
const LTE_READY: u32 = 1 << 5;
const LTE_READY_MS: u16 = 50;

// Linux logs a timeout of the ready poll, still does the access and returns -ETIMEDOUT; its coex
// callers (rtw8852b_common.c:1805 coex_init, coex.c:3746 cfg_gnt) ignore that, so Linux carries
// on. Here (as the JS proven on the Mini-PC) the timeout is the error, with the register and its
// last value, and nothing is written: kept on purpose until the Mini-PC shows a timeout (notes,
// check list item 3, desk decision).
fn lte_wait<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    bus.poll(
        Width::W8,
        R_AX_LTE_CTRL + 3,
        LTE_READY,
        LTE_READY,
        LTE_READY_MS,
    )?;
    Ok(())
}

// Linux: rtw89_mac_write_lte (mac.c:86)
pub fn write_lte<B: Bus + ?Sized>(bus: &mut B, offset: u32, val: u32) -> Rtw89Result<()> {
    lte_wait(bus)?;
    bus.w32(R_AX_LTE_WDATA, val)?;
    bus.w32(R_AX_LTE_CTRL, LTE_CMD_WRITE | offset)?;
    Ok(())
}

// Linux: rtw89_mac_read_lte (mac.c:102)
pub fn read_lte<B: Bus + ?Sized>(bus: &mut B, offset: u32) -> Rtw89Result<u32> {
    lte_wait(bus)?;
    bus.w32(R_AX_LTE_CTRL, LTE_CMD_READ | offset)?;
    Ok(bus.r32(R_AX_LTE_RDATA)?)
}

// Linux: rtw89_mac_coex_init (mac.c:6366), pta_mode RTK, direction INNER as
// __rtw8852bx_btc_init_cfg asks for. The 8852B is not 8851B/8852BT, so the PTA TX enable and the
// BTCCA response check apply. Each read-modify-write of Linux depends only on the old value, so
// it is one RMW op.
pub fn mac_coex_init<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    bus.set(Width::W8, R_AX_GPIO_MUXCFG, B_AX_ENBT)?;
    bus.set(Width::W8, R_AX_BTC_FUNC_EN, B_AX_PTA_WL_TX_EN)?;
    bus.set(Width::W8, R_AX_BT_COEX_CFG_2 + 1, B_AX_GNT_BT_POLARITY >> 8)?;
    bus.set(
        Width::W8,
        R_AX_CSR_MODE,
        B_AX_STATIS_BT_EN | B_AX_WL_ACT_MSK,
    )?;
    bus.set(Width::W8, R_AX_CSR_MODE + 2, B_AX_BT_CNT_RST >> 16)?;
    bus.clr(Width::W8, R_AX_TRXPTCL_RESP_0 + 3, B_AX_RSP_CHK_BTCCA >> 24)?;

    // read16, set BTCCA_EN, clear BTCCA_BRK_TXOP_EN, write16
    let cca = B_AX_BTCCA_EN | B_AX_BTCCA_BRK_TXOP_EN;
    bus.mask(
        Width::W16,
        R_AX_CCA_CFG_0,
        cca,
        B_AX_BTCCA_EN >> lowbit(cca),
    )?;

    let sw2 = read_lte(bus, R_AX_LTE_SW_CFG_2)?;
    write_lte(bus, R_AX_LTE_SW_CFG_2, sw2 & B_AX_WL_RX_CTRL)?;

    // RTW89_MAC_AX_COEX_RTK_MODE
    bus.mask(
        Width::W8,
        R_AX_GPIO_MUXCFG,
        B_AX_BTMODE_MASK,
        MAC_AX_BT_MODE_0_3,
    )?;
    bus.set(Width::W8, R_AX_TDMA_MODE, B_AX_RTK_BT_ENABLE)?;
    bus.mask(
        Width::W8,
        R_AX_BT_COEX_CFG_5,
        B_AX_BT_RPT_SAMPLE_RATE_MASK,
        MAC_AX_RTK_RATE,
    )?;

    // RTW89_MAC_AX_COEX_INNER: (val & ~BIT(2)) | BIT(1) on GPIO_MUXCFG + 1, i.e.
    // B_AX_PO_WIFI_PTA_PINS off and B_AX_PO_BT_PTA_PINS on
    let pins = (B_AX_PO_WIFI_PTA_PINS | B_AX_PO_BT_PTA_PINS) >> 8;
    bus.mask(
        Width::W8,
        R_AX_GPIO_MUXCFG + 1,
        pins,
        (B_AX_PO_BT_PTA_PINS >> 8) >> lowbit(pins),
    )?;
    Ok(())
}

// Linux: rtw89_mac_cfg_gnt (mac.c:6490) for S0 and S1; returns the word written
pub fn mac_cfg_gnt<B: Bus + ?Sized>(bus: &mut B, gnt: &[Gnt; 2]) -> Rtw89Result<u32> {
    // [bt val, bt ctrl, wl val, wl ctrl] of S0, S1: the RFC and BB bit of each
    const BITS: [[u32; 4]; 2] = [
        [
            B_AX_GNT_BT_RFC_S0_SW_VAL | B_AX_GNT_BT_BB_S0_SW_VAL,
            B_AX_GNT_BT_RFC_S0_SW_CTRL | B_AX_GNT_BT_BB_S0_SW_CTRL,
            B_AX_GNT_WL_RFC_S0_SW_VAL | B_AX_GNT_WL_BB_S0_SW_VAL,
            B_AX_GNT_WL_RFC_S0_SW_CTRL | B_AX_GNT_WL_BB_S0_SW_CTRL,
        ],
        [
            B_AX_GNT_BT_RFC_S1_SW_VAL | B_AX_GNT_BT_BB_S1_SW_VAL,
            B_AX_GNT_BT_RFC_S1_SW_CTRL | B_AX_GNT_BT_BB_S1_SW_CTRL,
            B_AX_GNT_WL_RFC_S1_SW_VAL | B_AX_GNT_WL_BB_S1_SW_VAL,
            B_AX_GNT_WL_RFC_S1_SW_CTRL | B_AX_GNT_WL_BB_S1_SW_CTRL,
        ],
    ];
    let mut val = 0;
    for (g, bits) in gnt.iter().zip(BITS.iter()) {
        let flags = [g.gnt_bt, g.gnt_bt_sw_en, g.gnt_wl, g.gnt_wl_sw_en];
        for (f, b) in flags.iter().zip(bits.iter()) {
            if *f != 0 {
                val |= b;
            }
        }
    }
    write_lte(bus, R_AX_LTE_SW_CFG_1, val)?;
    Ok(val)
}

// Linux: rtw89_mac_cfg_ctrl_path (mac.c:6637): BIT(2) of SYS_SDIO_CTRL + 3, read8/modify/write8
// of one bit, so a set or clear
pub fn mac_cfg_ctrl_path<B: Bus + ?Sized>(bus: &mut B, wl: bool) -> Rtw89Result<()> {
    let a = R_AX_SYS_SDIO_CTRL + 3;
    if wl {
        bus.set(Width::W8, a, 1 << 2)?;
    } else {
        bus.clr(Width::W8, a, 1 << 2)?;
    }
    Ok(())
}

// Linux: rtw89_mac_get_ctrl_path (mac.c:6667), the 8852B is an 885xB chip: whether the path is
// with WLAN, and the byte read
pub fn mac_get_ctrl_path<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<(bool, u32)> {
    let mask = B_AX_LTE_MUX_CTRL_PATH >> 24;
    let v = u32::from(bus.r8(R_AX_SYS_SDIO_CTRL + 3)?);
    Ok((field(v, mask) != 0, v))
}

/// The CMAC's state as rtw89_mac_check_mac_en_ax sees it: Ok(None) when it is on, else the value
pub(crate) fn cmac_off<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<Option<u32>> {
    let en = bus.r32(R_AX_CMAC_FUNC_EN)?;
    if en == RTW89_R32_EA || en == RTW89_R32_DEAD || en & B_AX_CMAC_EN == 0 {
        return Ok(Some(en));
    }
    Ok(None)
}

// Linux: rtw89_mac_cfg_plt_ax (mac.c:6583) for band 0 (rtw89_mac_reg_by_idx gives the base
// register), with rtw89_mac_check_mac_en_ax for the CMAC. `tx`/`rx`: RTW89_MAC_AX_PLT_* bits.
// Returns the CMAC value when it is off (nothing written).
pub fn mac_cfg_plt<B: Bus + ?Sized>(bus: &mut B, tx: u32, rx: u32) -> Rtw89Result<Option<u32>> {
    use crate::tables::btc::*;
    if let Some(v) = cmac_off(bus)? {
        return Ok(Some(v));
    }
    let p = |set: u32, bit: u32, reg: u32| if set & bit != 0 { reg } else { 0 };
    let val = p(tx, RTW89_MAC_AX_PLT_LTE_RX, B_AX_TX_PLT_GNT_LTE_RX)
        | p(tx, RTW89_MAC_AX_PLT_GNT_BT_TX, B_AX_TX_PLT_GNT_BT_TX)
        | p(tx, RTW89_MAC_AX_PLT_GNT_BT_RX, B_AX_TX_PLT_GNT_BT_RX)
        | p(tx, RTW89_MAC_AX_PLT_GNT_WL, B_AX_TX_PLT_GNT_WL)
        | p(rx, RTW89_MAC_AX_PLT_LTE_RX, B_AX_RX_PLT_GNT_LTE_RX)
        | p(rx, RTW89_MAC_AX_PLT_GNT_BT_TX, B_AX_RX_PLT_GNT_BT_TX)
        | p(rx, RTW89_MAC_AX_PLT_GNT_BT_RX, B_AX_RX_PLT_GNT_BT_RX)
        | p(rx, RTW89_MAC_AX_PLT_GNT_WL, B_AX_RX_PLT_GNT_WL)
        | B_AX_PLT_EN;
    bus.w16(R_AX_BT_PLT, val)?;
    Ok(None)
}

// Linux: rtw89_mac_cfg_sb (mac.c:6608), btc_sb.n[0].cfg = R_AX_SCOREBOARD. The new word depends
// only on the old one (its firmware bits 30:24), so one read-modify-write: keep the FW bits except
// BTGS1_NOTIFY, put in TP_MAJOR (the chip is powered, RTW89_FLAG_POWERON), the driver bits, the
// toggle. Returns the bits set.
pub fn mac_cfg_sb<B: Bus + ?Sized>(bus: &mut B, val: u32) -> Rtw89Result<u32> {
    let fw = B_MAC_AX_SB_FW_MASK;
    let drv = B_MAC_AX_SB_DRV_MASK;
    let fw_set = prep(fw, B_MAC_AX_BTGS1_NOTIFY | MAC_AX_NOTIFY_TP_MAJOR);
    let fw_keep = fw & !prep(fw, B_MAC_AX_BTGS1_NOTIFY) & !fw_set;
    let set_bits = B_AX_TOGGLE | prep(drv, field(val, drv)) | fw_set;
    bus.mask32(R_AX_SCOREBOARD, !fw_keep, set_bits)?;
    bus.delay_us(1000)?; // "avoid BT FW loss information"
    Ok(set_bits)
}

// Linux: rtw89_mac_get_sb (mac.c:6629): what the BT side wrote
pub fn mac_get_sb<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<u32> {
    Ok(bus.r32(R_AX_SCOREBOARD)?)
}

// ---- the chip's coex ops ----

// Linux: rtw8852b_btc_set_rfe (rtw8852b.c:751): rfe_type 1 is two antennas shared with BT (BTG)
pub fn btc_set_rfe(st: &mut BtcState, rfe: u32, cv: u32) {
    let md = &mut st.md;
    md.rfe_type = rfe;
    md.kt_ver = cv;
    md.bt_solo = 0;
    md.bt0_sw_type = BTC_SWITCH_INTERNAL;
    md.ant.num = if rfe > 0 {
        if rfe.is_multiple_of(2) { 3 } else { 2 }
    } else {
        2
    };
    md.ant.diversity = 0;
    md.ant.isolation = 10;
    if md.ant.num == 3 {
        md.ant.typ = BTC_ANT_DEDICATED;
        md.bt0_pos = BTC_BT_ALONE;
    } else {
        md.ant.typ = BTC_ANT_SHARED;
        md.bt0_pos = BTC_BT_BTG;
    }
    st.btg_pos = md.ant.btg_pos;
    st.ant_type = md.ant.typ;
}

// Linux: __rtw8852bx_btc_set_wl_pri (rtw8852b_common.c:1842); another map writes nothing
pub fn btc_set_wl_pri<B: Bus + ?Sized>(bus: &mut B, map: u32, state: bool) -> Rtw89Result<()> {
    let (reg, bits) = match map {
        BTC_PRI_MASK_TX_RESP => (R_BTC_BT_COEX_MSK_TABLE, B_BTC_PRI_MASK_TX_RESP_V1),
        BTC_PRI_MASK_BEACON => (R_AX_WL_PRI_MSK, B_AX_PTA_WL_PRI_MASK_BCNQ),
        BTC_PRI_MASK_RX_CCK => (R_BTC_BT_COEX_MSK_TABLE, B_BTC_PRI_MASK_RXCCK_V1),
        _ => return Ok(()),
    };
    if state {
        bus.set32(reg, bits)?;
    } else {
        bus.clr32(reg, bits)?;
    }
    Ok(())
}

// Linux: rtw8852bx_set_trx_mask (rtw8852b_common.c:1787)
fn set_trx_mask<B: Bus + ?Sized>(bus: &mut B, path: u32, group: u32, val: u32) -> Rtw89Result<()> {
    rf_w(bus, path, RR_LUTWE, RFREG_MASK, 0x20000)?;
    rf_w(bus, path, RR_LUTWA, RFREG_MASK, group)?;
    rf_w(bus, path, RR_LUTWD0, RFREG_MASK, val)?;
    rf_w(bus, path, RR_LUTWE, RFREG_MASK, 0x0)
}

// Linux: __rtw8852bx_btc_init_cfg (rtw8852b_common.c:1795); the BT_RX_GROUP masks are 8852BT only
pub fn btc_init_cfg<B: Bus + ?Sized>(bus: &mut B, st: &BtcState) -> Rtw89Result<()> {
    mac_coex_init(bus)?;
    btc_set_wl_pri(bus, BTC_PRI_MASK_TX_RESP, true)?;
    btc_set_wl_pri(bus, BTC_PRI_MASK_BEACON, true)?;
    // rf gnt debug off
    rf_w(bus, RF_PATH_A, RR_WLSEL, RFREG_MASK, 0x0)?;
    rf_w(bus, RF_PATH_B, RR_WLSEL, RFREG_MASK, 0x0)?;
    let masks = if st.ant_type == BTC_ANT_SHARED {
        [0x5ff, 0x5ff, 0x5ff, 0x55f]
    } else {
        [0x5df, 0x5df, 0x5ff, 0x5ff]
    };
    set_trx_mask(bus, RF_PATH_A, BTC_BT_SS_GROUP, masks[0])?;
    set_trx_mask(bus, RF_PATH_B, BTC_BT_SS_GROUP, masks[1])?;
    set_trx_mask(bus, RF_PATH_A, BTC_BT_TX_GROUP, masks[2])?;
    set_trx_mask(bus, RF_PATH_B, BTC_BT_TX_GROUP, masks[3])?;
    bus.w32(R_BTC_BREAK_TABLE, BTC_BREAK_PARAM)?;
    bus.set32(R_AX_CSR_MODE, B_AX_BT_CNT_RST | B_AX_STATIS_BT_EN)?;
    Ok(())
}

// Linux: rtw8852b_btc_set_wl_txpwr_ctrl (rtw8852b.c:801) with rtw89_mac_txpwr_write32_mask: both
// registers lie in the CMAC0 TX power range, so get_txpwr_cr gives them unchanged. Linux passes
// an already shifted value to rtw89_write32_mask, which shifts again by the mask's lowest bit
// (bit 1 for PWR_COEXT): kept.
pub fn btc_set_wl_txpwr_ctrl<B: Bus + ?Sized>(bus: &mut B, txpwr_val: u32) -> Rtw89Result<()> {
    let all_time = txpwr_val & 0xffff;
    let gnt_bt = txpwr_val >> 16;
    let s9 = |v: u32| (((v << 23) as i32) >> 23) as u32; // s16 data:9
    let mut write = |reg: u32, msk: u32, val: u32, en: u32, cond: bool| -> Rtw89Result<()> {
        let mut wrt = val.wrapping_shl(lowbit(msk)) & msk;
        wrt = if cond { wrt | en } else { wrt & !en };
        bus.mask32(reg, msk | en, wrt)?;
        Ok(())
    };
    write(
        R_AX_PWR_RATE_CTRL,
        B_AX_FORCE_PWR_BY_RATE_VALUE_MASK,
        if all_time == 0xffff { 0 } else { s9(all_time) },
        B_AX_FORCE_PWR_BY_RATE_EN,
        all_time != 0xffff,
    )?;
    write(
        R_AX_PWR_COEXT_CTRL,
        B_AX_TXAGC_BT_MASK,
        if gnt_bt == 0xffff { 0 } else { s9(gnt_bt) },
        B_AX_TXAGC_BT_EN,
        gnt_bt != 0xffff,
    )
}

// Linux: __rtw8852bx_btc_get_bt_rssi (rtw8852b_common.c:1871): +6 offset, s8 clamp
pub fn btc_get_bt_rssi(val: i32) -> i8 {
    let s8 = |v: i32| v as i8;
    let v = i32::from(s8(val.wrapping_add(6))).clamp(-100, 0);
    s8(v + 100)
}

// Linux: __rtw8852bx_btc_update_bt_cnt (rtw8852b_common.c:1878): moved to the firmware
pub fn btc_update_bt_cnt() {}

// Linux: __rtw8852bx_btc_wl_s1_standby (rtw8852b_common.c:1883)
pub fn btc_wl_s1_standby<B: Bus + ?Sized>(bus: &mut B, state: bool) -> Rtw89Result<()> {
    rf_w(bus, RF_PATH_B, RR_LUTWE, RFREG_MASK, 0x80000)?;
    rf_w(bus, RF_PATH_B, RR_LUTWA, RFREG_MASK, 0x1)?;
    rf_w(bus, RF_PATH_B, RR_LUTWD1, RFREG_MASK, 0x31)?;
    rf_w(
        bus,
        RF_PATH_B,
        RR_LUTWD0,
        RFREG_MASK,
        if state { 0x179 } else { 0x20 },
    )?;
    rf_w(bus, RF_PATH_B, RR_LUTWE, RFREG_MASK, 0x0)
}

// Linux: rtw8852bx_btc_set_wl_lna2 (rtw8852b_common.c:1898): level 0 default, 1 LNA2 fixed at 5;
// another level writes nothing
fn set_wl_lna2<B: Bus + ?Sized>(bus: &mut B, level: u32) -> Rtw89Result<()> {
    if level > 1 {
        return Ok(());
    }
    let odd = if level == 0 { 0x17 } else { 0x5 };
    rf_w(bus, RF_PATH_B, RR_LUTWE, RFREG_MASK, 0x1000)?;
    for (a, v) in [(0x0, 0x15), (0x1, odd), (0x2, 0x15), (0x3, odd)] {
        rf_w(bus, RF_PATH_B, RR_LUTWA, RFREG_MASK, a)?;
        rf_w(bus, RF_PATH_B, RR_LUTWD0, RFREG_MASK, v)?;
    }
    rf_w(bus, RF_PATH_B, RR_LUTWE, RFREG_MASK, 0x0)
}

// Linux: __rtw8852bx_ctrl_nbtg_bt_tx (rtw8852b_common.c:1601), the chip op for PHY 0; the pre-AGC
// tables and their writes are phy's
pub fn btc_ctrl_nbtg_bt_tx<B: Bus + ?Sized>(bus: &mut B, en: bool) -> Rtw89Result<()> {
    ctrl_nbtg_bt_tx(bus, en)
}

// Linux: __rtw8852bx_btc_set_wl_rx_gain (rtw8852b_common.c:1928): level 1 FDD free-run, 2 BTG
// co-rx, else the default
pub fn btc_set_wl_rx_gain<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    level: u32,
) -> Rtw89Result<()> {
    btc_ctrl_nbtg_bt_tx(bus, level == 1)?;
    st.dm.wl_lna2 = u32::from(level == 2);
    set_wl_lna2(bus, st.dm.wl_lna2)
}
