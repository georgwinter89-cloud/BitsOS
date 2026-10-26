//! Ports and roles of the RTL8852BE MAC (JS js/macinit-port.mjs; Linux mac.c
//! rtw89_mac_port_update, rtw89_mac_vif_init), the RX filter, PPDU status and
//! the RTS threshold. Band (mac_idx) 0 only, so rtw89_mac_reg_by_idx is the
//! plain base address.

use crate::bus::{Bus, Width, prep};
use crate::error::{Rtw89Error, Rtw89Result};
use crate::fw::FwState;
use crate::pci::PciRings;
use crate::tables::regs::*;

use super::h2c;
use super::io::{MacSel, check_mac_en, rmw32};
use super::{MacEvent, MacState};

/// mac.c:4547-4555, the #defines of the port update
const BCN_INTERVAL: u32 = 100;
const BCN_ERLY_DEF: u32 = 160;
const BCN_SETUP_DEF: u32 = 2;
const BCN_HOLD_DEF: u32 = 200;
const BCN_MASK_DEF: u32 = 0;
const TBTT_ERLY_DEF: u32 = 5;
const TBTT_AGG_DEF: u32 = 1;
const BCN_ERLY_SET_DLY: u32 = 10 * 2;
/// The longest msleep(beacon_int + 1) BitsOS waits on any thread (spec/wlan.md §2.3: every
/// wait at most 10 s; the AP's beacon interval is a u16 of TUs, so up to 65 s in Linux).
/// A thread's own bound is `MacWaits::func_sw_ms` (B4), never above this.
pub const FUNC_SW_MAX_WAIT_MS: u32 = 10_000;

/// The fields of struct rtw89_vif_link and its bss_conf that the port update reads
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vif {
    pub mac_id: u8,
    pub port: u8,
    pub mac_idx: u8,
    /// RTW89_NET_TYPE_*
    pub net_type: u32,
    /// TUs (bss_conf->beacon_int); 0 before an association
    pub beacon_int: u16,
    pub dtim_period: u8,
    pub bss_color: u8,
    pub nontransmitted: bool,
    pub bssid_index: u8,
    pub addr: [u8; 6],
}

impl Vif {
    // Linux: rtw89_init_vif (core.c:6795) and rtw89_vif_type_mapping (core.c:5676) for a
    // station that is not associated (JS portVif's defaults)
    pub const fn station(mac_id: u8, port: u8, addr: [u8; 6]) -> Vif {
        Vif {
            mac_id,
            port,
            mac_idx: 0,
            net_type: RTW89_NET_TYPE_NO_LINK,
            beacon_int: 0,
            dtim_period: 0,
            bss_color: 0,
            nontransmitted: false,
            bssid_index: 0,
            addr,
        }
    }
    fn is(&self, t: u32) -> bool {
        self.net_type == t
    }
}

// rtw89_mac_reg_by_port (mac.h:1212) on rtw89_port_base_ax: one block of 0x40 per port
fn port_reg(vif: &Vif, base: u32) -> u32 {
    base + u32::from(vif.port) * 0x40
}

fn set_or_clr<B: Bus + ?Sized>(bus: &mut B, reg: u32, on: bool, bits: u32) -> Rtw89Result<()> {
    if on {
        bus.set32(reg, bits)?;
    } else {
        bus.clr32(reg, bits)?;
    }
    Ok(())
}

// Linux: rtw89_mac_port_cfg_func_sw (mac.c:4557); 8852B: no 8852A backup or TBTT clears.
// An AP port would need rtw89_mac_bcn_drop first: station only, refused before any write.
fn cfg_func_sw<B: Bus + ?Sized>(bus: &mut B, m: &mut MacState, vif: &Vif) -> Rtw89Result<()> {
    let cfg = port_reg(vif, R_AX_PORT_CFG_P0);
    if bus.r32(cfg)? & B_AX_PORT_FUNC_EN == 0 {
        return Ok(());
    }
    if vif.is(RTW89_NET_TYPE_AP_MODE) {
        return Err(Rtw89Error::Invalid {
            what: "port update: AP mode (rtw89_mac_bcn_drop) is not ported",
            value: vif.net_type,
        });
    }
    let want = u32::from(vif.beacon_int) + 1;
    let ms = want.min(m.waits.func_sw_ms.min(FUNC_SW_MAX_WAIT_MS));
    if ms != want {
        m.push(MacEvent::WaitClamped {
            what: "port func_sw msleep(beacon_int + 1)",
            want_ms: want,
            ms,
        });
    }
    bus.delay_us(ms * 1000)?; // msleep(beacon_int + 1)
    bus.clr32(cfg, B_AX_PORT_FUNC_EN | B_AX_BRK_SETUP)?;
    bus.set32(cfg, B_AX_TSFTR_RST)?;
    bus.w32(port_reg(vif, R_AX_BCN_CNT_TMR_P0), 0)?;
    Ok(())
}

/// B_AX_BSS_COLOB_AX_PORT_n_MASK of rtw89_mac_port_cfg_bss_color (mac.c:4867)
const BSS_COLOR_MASK: [u32; RTW89_PORT_NUM as usize] = [
    B_AX_BSS_COLOB_AX_PORT_0_MASK,
    B_AX_BSS_COLOB_AX_PORT_1_MASK,
    B_AX_BSS_COLOB_AX_PORT_2_MASK,
    B_AX_BSS_COLOB_AX_PORT_3_MASK,
    B_AX_BSS_COLOB_AX_PORT_4_MASK,
];

// Linux: rtw89_mac_port_update (mac.c:5104) with its rtw89_mac_port_cfg_* helpers (4557-4953)
pub fn port_update<B: Bus + ?Sized>(bus: &mut B, m: &mut MacState, vif: &Vif) -> Rtw89Result<()> {
    let Some(&color_mask) = BSS_COLOR_MASK.get(usize::from(vif.port)) else {
        return Err(Rtw89Error::Invalid {
            what: "port update: port past RTW89_PORT_NUM",
            value: vif.port.into(),
        });
    };
    let cfg = port_reg(vif, R_AX_PORT_CFG_P0);
    let infra_or_adhoc = vif.is(RTW89_NET_TYPE_INFRA) || vif.is(RTW89_NET_TYPE_AD_HOC);
    let ap = vif.is(RTW89_NET_TYPE_AP_MODE);
    cfg_func_sw(bus, m, vif)?;
    bus.clr32(cfg, B_AX_TXBCN_RPT_EN)?; // cfg_tx_rpt(false)
    bus.clr32(cfg, B_AX_RXBCN_RPT_EN)?; // cfg_rx_rpt(false)
    bus.mask32(cfg, B_AX_NET_TYPE_MASK, vif.net_type)?; // cfg_net_type
    set_or_clr(
        bus,
        cfg,
        !vif.is(RTW89_NET_TYPE_NO_LINK),
        B_AX_TBTT_PROHIB_EN | B_AX_BRK_SETUP,
    )?; // cfg_bcn_prct
    set_or_clr(bus, cfg, infra_or_adhoc, B_AX_RX_BSSID_FIT_EN)?; // cfg_rx_sw
    set_or_clr(bus, cfg, infra_or_adhoc, B_AX_TSF_UDT_EN)?; // cfg_rx_sync_by_nettype
    set_or_clr(bus, cfg, ap || vif.is(RTW89_NET_TYPE_AD_HOC), B_AX_BCNTX_EN)?; // cfg_tx_sw_by_nettype
    let intv = if vif.beacon_int != 0 {
        u32::from(vif.beacon_int)
    } else {
        BCN_INTERVAL
    };
    bus.mask32(
        port_reg(vif, R_AX_BCN_SPACE_CFG_P0),
        B_AX_BCN_SPACE_MASK,
        intv,
    )?; // cfg_bcn_intv
    // cfg_hiq_win: hiq_win[port] of rtw89_port_base_ax
    let hiq = if vif.port == 0 {
        R_AX_P0MB_HGQ_WINDOW_CFG_0
    } else {
        R_AX_PORT_HGQ_WINDOW_CFG + u32::from(vif.port) - 1
    };
    bus.w8(hiq, if ap { 16 } else { 0 })?;
    // cfg_hiq_dtim
    bus.set(
        Width::W8,
        R_AX_MD_TSFT_STMP_CTL,
        B_AX_UPD_HGQMD | B_AX_UPD_TIMIE,
    )?;
    bus.mask(
        Width::W16,
        port_reg(vif, R_AX_DTIM_CTRL_P0),
        B_AX_DTIM_NUM_MASK,
        vif.dtim_period.into(),
    )?;
    // cfg_hiq_drop: read, clear this port's drop bit (and bit 0 for port 0), write
    let drop = prep(B_AX_PORT_DROP_4_0_MASK, 1 << vif.port) | u32::from(vif.port == 0);
    bus.clr32(R_AX_MBSSID_DROP_0, drop)?;
    let prohib = port_reg(vif, R_AX_TBTT_PROHIB_P0);
    bus.mask32(prohib, B_AX_TBTT_SETUP_MASK, BCN_SETUP_DEF)?; // cfg_bcn_setup_time
    bus.mask32(prohib, B_AX_TBTT_HOLD_MASK, BCN_HOLD_DEF)?; // cfg_bcn_hold_time
    bus.mask32(
        port_reg(vif, R_AX_BCN_AREA_P0),
        B_AX_BCN_MSK_AREA_MASK,
        BCN_MASK_DEF,
    )?; // cfg_bcn_mask_area
    bus.mask(
        Width::W16,
        port_reg(vif, R_AX_TBTTERLYINT_CFG_P0),
        B_AX_TBTTERLY_MASK,
        TBTT_ERLY_DEF,
    )?; // cfg_tbtt_early
    bus.mask(
        Width::W16,
        port_reg(vif, R_AX_TBTT_AGG_P0),
        B_AX_TBTT_AGG_NUM_MASK,
        TBTT_AGG_DEF,
    )?; // cfg_tbtt_agg
    // cfg_bss_color: ports 0-3 live in PTCL_BSS_COLOR_0, port 4 in the next register
    let color_reg = R_AX_PTCL_BSS_COLOR_0 + if vif.port >= 4 { 4 } else { 0 };
    bus.mask32(color_reg, color_mask, vif.bss_color.into())?;
    // cfg_mbssid
    if !ap && vif.port == 0 {
        bus.clr32(R_AX_MBSSID_CTRL, B_AX_P0MB_ALL_MASK)?;
    }
    bus.set32(cfg, B_AX_PORT_FUNC_EN)?; // cfg_func_en(true)
    // rtw89_mac_port_tsf_resync_all: nothing without an AP interface
    bus.delay_us(BCN_ERLY_SET_DLY)?; // fsleep(BCN_ERLY_SET_DLY)
    bus.mask32(
        port_reg(vif, R_AX_BCNERLYINT_CFG_P0),
        B_AX_BCNERLY_MASK,
        BCN_ERLY_DEF,
    )?; // cfg_bcn_early
    let bcaid = if vif.nontransmitted {
        u32::from(vif.bssid_index)
    } else {
        0
    };
    bus.mask32(
        R_AX_BCN_PSR_RPT_P0 + u32::from(vif.port) * 4,
        B_AX_BCAID_P0_MASK,
        bcaid,
    )?; // cfg_bcn_psr_rpt
    bus.flush()?;
    Ok(())
}

// Linux: rtw89_mac_dmac_tbl_init (mac.c:4403); 8852B: AX and no secure boot
fn dmac_tbl_init<B: Bus + ?Sized>(bus: &mut B, mac_id: u8) -> Rtw89Result<()> {
    for i in 0..4u32 {
        bus.w32(
            R_AX_FILTER_MODEL_ADDR,
            DMAC_TBL_BASE_ADDR + (u32::from(mac_id) << 4) + (i << 2),
        )?;
        bus.w32(R_AX_INDIR_ACCESS_ENTRY, 0)?;
    }
    Ok(())
}

/// rtw89_mac_cmac_tbl_init's eight dwords (mac.c:4418), in their order
const CMAC_TBL_INIT: [u32; 8] = [0x4, 0x400A_0004, 0, 0, 0, 0xE43_000B, 0, 0xB_8109];

// Linux: rtw89_mac_cmac_tbl_init (mac.c:4418)
fn cmac_tbl_init<B: Bus + ?Sized>(bus: &mut B, mac_id: u8) -> Rtw89Result<()> {
    bus.w32(
        R_AX_FILTER_MODEL_ADDR,
        CMAC_TBL_BASE_ADDR + u32::from(mac_id) * CCTL_INFO_SIZE,
    )?;
    for (i, v) in (0u32..).zip(CMAC_TBL_INIT) {
        bus.w32(R_AX_INDIR_ACCESS_ENTRY + i * 4, v)?;
    }
    Ok(())
}

// Linux: rtw89_mac_set_macid_pause (mac.c:4437) up to its H2C: the CMAC check, then
// rtw89_fw_h2c_macid_pause through `send(bus, sh, grp, pause)`. No CHANGING_INTERFACE
// check (P2P only).
pub fn macid_pause_with<B: Bus + ?Sized>(
    bus: &mut B,
    mac_id: u8,
    pause: bool,
    send: impl FnOnce(&mut B, u32, u32, bool) -> Rtw89Result<()>,
) -> Rtw89Result<()> {
    check_mac_en(bus, MacSel::Cmac)?;
    send(bus, u32::from(mac_id & 0x1f), u32::from(mac_id >> 5), pause)
}

// Linux: rtw89_mac_set_macid_pause (mac.c:4437) with this module's MACID pause H2C
pub fn macid_pause<B: Bus + ?Sized>(
    bus: &mut B,
    rings: &mut PciRings,
    fw: &mut FwState,
    mac_id: u8,
    pause: bool,
) -> Rtw89Result<()> {
    macid_pause_with(bus, mac_id, pause, |b, sh, grp, p| {
        h2c::h2c_macid_pause(b, rings, fw, sh, grp, p).map(|_| ())
    })
}

/// The steps of rtw89_mac_vif_init that are sta's (cam.c and their H2Cs, JS CONTRACTS.md):
/// sta (WL-3) implements them; each gets the bus, in rtw89_mac_vif_init's order
pub trait VifHooks<B: Bus + ?Sized> {
    /// rtw89_fw_h2c_macid_pause(sh, grp, pause) (macinit's `h2c::h2c_macid_pause`)
    fn macid_pause(&mut self, bus: &mut B, sh: u32, grp: u32, pause: bool) -> Rtw89Result<()>;
    /// rtw89_fw_h2c_role_maintain(.., upd)
    fn role_maintain(&mut self, bus: &mut B, upd: u32) -> Rtw89Result<()>;
    /// rtw89_fw_h2c_join_info(.., dis_conn)
    fn join_info(&mut self, bus: &mut B, dis_conn: bool) -> Rtw89Result<()>;
    /// rtw89_cam_init
    fn cam_init(&mut self, bus: &mut B) -> Rtw89Result<()>;
    /// rtw89_fw_h2c_cam(.., upd)
    fn addr_cam(&mut self, bus: &mut B, upd: u32) -> Rtw89Result<()>;
    /// rtw89_chip_h2c_default_cmac_tbl
    fn default_cmac_tbl(&mut self, bus: &mut B) -> Rtw89Result<()>;
}

// Linux: rtw89_mac_vif_init (mac.c:5045) for a station; rtw89_chip_h2c_default_dmac_tbl
// is none for the 8852B
pub fn vif_init<B: Bus + ?Sized, H: VifHooks<B> + ?Sized>(
    bus: &mut B,
    m: &mut MacState,
    vif: &Vif,
    hooks: &mut H,
) -> Rtw89Result<()> {
    port_update(bus, m, vif)?;
    dmac_tbl_init(bus, vif.mac_id)?;
    cmac_tbl_init(bus, vif.mac_id)?;
    macid_pause_with(bus, vif.mac_id, false, |b, sh, grp, p| {
        hooks.macid_pause(b, sh, grp, p)
    })?;
    hooks.role_maintain(bus, RTW89_ROLE_CREATE)?;
    hooks.join_info(bus, true)?;
    hooks.cam_init(bus)?;
    hooks.addr_cam(bus, RTW89_ROLE_CREATE)?;
    hooks.default_cmac_tbl(bus)?;
    bus.flush()?;
    Ok(())
}

// Linux: DEFAULT_AX_RX_FLTR (reg.h:3348), hal.rx_fltr as rtw89_core_init sets it (core.c:7020)
pub const fn default_rx_fltr() -> u32 {
    B_AX_A_A1_MATCH
        | B_AX_A_BC
        | B_AX_A_MC
        | B_AX_A_UC_CAM_MATCH
        | B_AX_A_BC_CAM_MATCH
        | B_AX_A_PWR_MGNT
        | B_AX_A_FTM_REQ
        | B_AX_A_BCN_CHK_EN
        | prep(B_AX_UID_FILTER_MASK, 3)
}

/// B_AX_RX_FLTR_CFG_MASK: all but the MPDU length field, so one RMW keeps that field
const RX_FLTR_CFG_MASK: u32 = !B_AX_RX_MPDU_MAX_LEN_MASK;

// Linux: rtw89_mac_set_rx_fltr (mac.c:2678)
pub fn set_rx_fltr<B: Bus + ?Sized>(bus: &mut B, rx_fltr: u32) -> Rtw89Result<()> {
    rmw32(
        bus,
        R_AX_RX_FLTR_OPT,
        RX_FLTR_CFG_MASK,
        rx_fltr & RX_FLTR_CFG_MASK,
    )
}

/// mac80211's filter flags rtw89_ops_configure_filter looks at (FIF_*); mac80211 sets
/// `bcn_prbresp_promisc` while it scans
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FilterFlags {
    pub allmulti: bool,
    pub fcsfail: bool,
    pub other_bss: bool,
    pub bcn_prbresp_promisc: bool,
    pub probe_req: bool,
}

// Linux: rtw89_ops_configure_filter (mac80211.c:308) for mac80211's new filter flags;
// `scanning` = rtwdev->scanning. Updates hal.rx_fltr (`m.rx_fltr`) and the last flags
// (`m.fif`), writes the value (with the scan's bits off while scanning) and returns it.
pub fn rx_filter<B: Bus + ?Sized>(
    bus: &mut B,
    m: &mut MacState,
    flags: FilterFlags,
    scanning: bool,
) -> Rtw89Result<u32> {
    let old = m.fif;
    let mut rx = m.rx_fltr;
    let mut upd = |on: bool, b: u32| {
        rx = if on { rx | b } else { rx & !b };
    };
    let scan_bits = B_AX_A_BCN_CHK_EN | B_AX_A_BC | B_AX_A_A1_MATCH;
    if old.allmulti != flags.allmulti {
        upd(!flags.allmulti, B_AX_A_MC);
    }
    if old.fcsfail != flags.fcsfail {
        upd(flags.fcsfail, B_AX_A_CRC32_ERR);
    }
    if old.other_bss != flags.other_bss {
        upd(!flags.other_bss, B_AX_A_A1_MATCH);
    }
    if old.bcn_prbresp_promisc != flags.bcn_prbresp_promisc {
        upd(!flags.bcn_prbresp_promisc, scan_bits);
    }
    if old.probe_req != flags.probe_req {
        upd(!flags.probe_req, B_AX_A_BC_CAM_MATCH | B_AX_A_UC_CAM_MATCH);
    }
    upd(false, B_AX_SNIFFER_MODE); // never IEEE80211_CONF_MONITOR
    m.rx_fltr = rx;
    m.fif = flags;
    let out = if scanning { rx & !scan_bits } else { rx };
    set_rx_fltr(bus, out)?;
    bus.flush()?;
    Ok(out)
}

// Linux: rtw89_mac_cfg_ppdu_status_ax (mac.c:6290), MAC 0
pub fn cfg_ppdu_status<B: Bus + ?Sized>(bus: &mut B, enable: bool) -> Rtw89Result<()> {
    check_mac_en(bus, MacSel::Cmac)?;
    if !enable {
        bus.clr32(R_AX_PPDU_STAT, B_AX_PPDU_STAT_RPT_EN)?;
        return Ok(());
    }
    bus.w32(
        R_AX_PPDU_STAT,
        B_AX_PPDU_STAT_RPT_EN
            | B_AX_APP_MAC_INFO_RPT
            | B_AX_APP_PLCP_HDR_RPT
            | B_AX_PPDU_STAT_RPT_CRC32,
    )?;
    bus.mask32(
        R_AX_HW_RPT_FWD,
        B_AX_FWD_PPDU_STAT_MASK,
        RTW89_PRPT_DEST_HOST,
    )?;
    Ok(())
}

/// mac.c:6317-6322, the #defines of __rtw89_mac_update_rts_threshold
const MAC_AX_TIME_TH_SH: u32 = 5;
const MAC_AX_LEN_TH_SH: u32 = 4;
const MAC_AX_TIME_TH_MAX: u32 = 255;
const MAC_AX_LEN_TH_MAX: u32 = 255;
const MAC_AX_TIME_TH_DEF: u32 = 88;
const MAC_AX_LEN_TH_DEF: u32 = 4080;

// Linux: __rtw89_mac_update_rts_threshold (mac.c:6315); wiphy rts_threshold is (u32)-1
// (never set), so the defaults
pub fn update_rts_threshold<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    let time_th = (MAC_AX_TIME_TH_DEF >> MAC_AX_TIME_TH_SH).min(MAC_AX_TIME_TH_MAX);
    let len_th = (MAC_AX_LEN_TH_DEF >> MAC_AX_LEN_TH_SH).min(MAC_AX_LEN_TH_MAX);
    bus.mask(
        Width::W16,
        R_AX_AGG_LEN_HT_0,
        B_AX_RTS_TXTIME_TH_MASK,
        time_th,
    )?;
    bus.mask(Width::W16, R_AX_AGG_LEN_HT_0, B_AX_RTS_LEN_TH_MASK, len_th)?;
    Ok(())
}
