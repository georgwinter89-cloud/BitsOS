//! The MAC's system and TRX init of the RTL8852BE after the firmware runs (JS
//! js/macinit-trx.mjs; Linux mac.c sys_init_ax, dmac_init_ax, cmac_init_ax,
//! trx_init_ax), band 0 only. The 8852B is one of the "885xB" chips of
//! rtw89_is_rtl885xb.

use crate::bus::{Bus, Width, prep};
use crate::error::{Rtw89Error, Rtw89Result};
use crate::pci;
use crate::tables::regs::*;

use super::imr;
use super::io::{MacSel, check_mac_en, poll_warn, rmw16, rmw32, us_to_ms};
use super::{MacEvent, MacState};

/// TRXCFG_WAIT_CNT (2000 us) as a poll op's milliseconds
const TRXCFG_WAIT_MS: u16 = us_to_ms(TRXCFG_WAIT_CNT);

// Linux: sys_init_ax (mac.c:1701) with dmac_func_en_ax (1656), cmac_func_en_ax(0, true)
// (1610) and chip_func_en_ax (1690)
pub fn sys_init<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    // dmac_func_en_ax, 8852B branch (not 8852C): DLE_WDE/PLE_EN are left out on purpose
    bus.w32(
        R_AX_DMAC_FUNC_EN,
        B_AX_MAC_FUNC_EN
            | B_AX_DMAC_FUNC_EN
            | B_AX_MAC_SEC_EN
            | B_AX_DISPATCHER_EN
            | B_AX_DLE_CPUIO_EN
            | B_AX_PKT_IN_EN
            | B_AX_DMAC_TBL_EN
            | B_AX_PKT_BUF_EN
            | B_AX_STA_SCH_EN
            | B_AX_TXPKT_CTRL_EN
            | B_AX_WD_RLS_EN
            | B_AX_MPDU_PROC_EN
            | B_AX_DMAC_CRPRT,
    )?;
    // 8852B: no AXIDMA_CLK_EN (8852BT only)
    bus.w32(
        R_AX_DMAC_CLK_EN,
        B_AX_MAC_SEC_CLK_EN
            | B_AX_DISPATCHER_CLK_EN
            | B_AX_DLE_CPUIO_CLK_EN
            | B_AX_PKT_IN_CLK_EN
            | B_AX_STA_SCH_CLK_EN
            | B_AX_TXPKT_CTRL_CLK_EN
            | B_AX_WD_RLS_CLK_EN
            | B_AX_BBRPT_CLK_EN,
    )?;
    // cmac_func_en_ax for MAC 0: clocks first, then functions
    bus.set32(
        R_AX_CK_EN,
        B_AX_CMAC_CKEN
            | B_AX_PHYINTF_CKEN
            | B_AX_CMAC_DMA_CKEN
            | B_AX_PTCLTOP_CKEN
            | B_AX_SCHEDULER_CKEN
            | B_AX_TMAC_CKEN
            | B_AX_RMAC_CKEN,
    )?;
    bus.set32(
        R_AX_CMAC_FUNC_EN,
        B_AX_CMAC_EN
            | B_AX_CMAC_TXEN
            | B_AX_CMAC_RXEN
            | B_AX_PHYINTF_EN
            | B_AX_CMAC_DMA_EN
            | B_AX_PTCLTOP_EN
            | B_AX_SCHEDULER_EN
            | B_AX_TMAC_EN
            | B_AX_RMAC_EN
            | B_AX_CMAC_CRPRT,
    )?;
    // chip_func_en_ax: 8852B is 885xB
    bus.set32(R_AX_SPS_DIG_ON_CTRL0, B_AX_OCP_L1_MASK)?;
    Ok(())
}

// Linux: sta_sch_init_ax (mac.c:2425); _patch_ss2f_path does nothing on 885xB
fn sta_sch_init<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    check_mac_en(bus, MacSel::Dmac)?;
    bus.set(Width::W8, R_AX_SS_CTRL, B_AX_SS_EN)?;
    bus.poll32(
        R_AX_SS_CTRL,
        B_AX_SS_INIT_DONE_1,
        B_AX_SS_INIT_DONE_1,
        TRXCFG_WAIT_MS,
    )?;
    bus.set32(R_AX_SS_CTRL, B_AX_SS_WARM_INIT_FLG)?;
    bus.clr32(R_AX_SS_CTRL, B_AX_SS_NONEMPTY_SS2FINFO_EN)?;
    Ok(())
}

// Linux: mpdu_proc_init_ax (mac.c:2458)
fn mpdu_proc_init<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    check_mac_en(bus, MacSel::Dmac)?;
    bus.w32(R_AX_ACTION_FWD0, TRXCFG_MPDU_PROC_ACT_FRWD)?;
    bus.w32(R_AX_TF_FWD, TRXCFG_MPDU_PROC_TF_FRWD)?;
    bus.set32(R_AX_MPDU_PROC, B_AX_APPEND_FCS | B_AX_A_ICV_ERR)?;
    bus.w32(R_AX_CUT_AMSDU_CTRL, TRXCFG_MPDU_PROC_CUT_CTRL)?;
    Ok(())
}

// Linux: sec_eng_init_ax (mac.c:2479)
fn sec_eng_init<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    check_mac_en(bus, MacSel::Dmac)?;
    // 8852B: no UC_MGNT_DEC (8852C only), TX_PARTIAL_MODE cleared
    rmw32(
        bus,
        R_AX_SEC_ENG_CTRL,
        B_AX_TX_PARTIAL_MODE,
        B_AX_CLK_EN_CGCMP
            | B_AX_CLK_EN_WAPI
            | B_AX_CLK_EN_WEP_TKIP
            | B_AX_SEC_TX_ENC
            | B_AX_SEC_RX_DEC
            | B_AX_MC_DEC
            | B_AX_BC_DEC,
    )?;
    bus.set32(R_AX_SEC_MPDU_PROC, B_AX_APPEND_ICV | B_AX_APPEND_MIC)?;
    Ok(())
}

// Linux: dmac_init_ax (mac.c:2521) for MAC 0: rtw89_mac_dle_init with ext mode
// RTW89_QTA_INVALID (the mode only matters for DLFW), rtw89_mac_preload_init (returns
// at once for 885xB), rtw89_mac_hfc_init(true, true, true), then the three inits
pub fn dmac_init<B: Bus + ?Sized>(bus: &mut B, m: &mut MacState) -> Rtw89Result<()> {
    let mode = m.dmac.qta_mode;
    pci::dle_init(bus, &mut m.dmac, mode, mode)?;
    pci::hfc_init(bus, &mut m.dmac, true, true, true)?;
    sta_sch_init(bus)?;
    mpdu_proc_init(bus)?;
    sec_eng_init(bus)?;
    Ok(())
}

// Linux: scheduler_init_ax (mac.c:2591)
fn scheduler_init<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    check_mac_en(bus, MacSel::Cmac)?;
    // 8852B: not _V1
    bus.mask32(
        R_AX_PREBKF_CFG_1,
        B_AX_SIFS_MACTXEN_T1_MASK,
        SIFS_MACTXEN_T1,
    )?;
    bus.set32(R_AX_SCH_EXT_CTRL, B_AX_PORT_RST_TSF_ADV)?; // 885xB only
    bus.clr32(R_AX_CCA_CFG_0, B_AX_BTCCA_EN)?;
    bus.mask32(R_AX_PREBKF_CFG_0, B_AX_PREBKF_TIME_MASK, SCH_PREBKF_24US)?; // 8852B: always
    Ok(())
}

// Linux: addr_cam_init_ax (mac.c:2564): val |= range, clear, enable (an OR), then the
// clear bit polled as a 16-bit read
fn addr_cam_init<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    check_mac_en(bus, MacSel::Cmac)?;
    bus.set32(
        R_AX_ADDR_CAM_CTRL,
        prep(B_AX_ADDR_CAM_RANGE_MASK, 0x7f) | B_AX_ADDR_CAM_CLR | B_AX_ADDR_CAM_EN,
    )?;
    bus.poll(
        Width::W16,
        R_AX_ADDR_CAM_CTRL,
        B_AX_ADDR_CAM_CLR,
        0,
        TRXCFG_WAIT_MS,
    )?;
    Ok(())
}

// Linux: rx_fltr_init_ax (mac.c:2692) with rtw89_mac_typ_fltr_opt_ax(MGNT..DATA, TO_HOST)
fn rx_fltr_init<B: Bus + ?Sized>(bus: &mut B, rx_fltr: u32) -> Rtw89Result<()> {
    check_mac_en(bus, MacSel::Cmac)?;
    for reg in [R_AX_MGNT_FLTR, R_AX_CTRL_FLTR, R_AX_DATA_FLTR] {
        bus.w32(reg, RX_FLTR_FRAME_TO_HOST)?;
    }
    bus.w32(R_AX_RX_FLTR_OPT, rx_fltr)?;
    bus.w16(
        R_AX_PLCP_HDR_FLTR,
        B_AX_CCK_CRC_CHK
            | B_AX_CCK_SIG_CHK
            | B_AX_LSIG_PARITY_CHK_EN
            | B_AX_SIGA_CRC_CHK
            | B_AX_VHT_SU_SIGB_CRC_CHK
            | B_AX_VHT_MU_SIGB_CRC_CHK
            | B_AX_HE_SIGB_CRC_CHK,
    )?;
    Ok(())
}

// Linux: cca_ctrl_init_ax (mac.c:2758) with _patch_dis_resp_chk (2724; 8852B: clear the checks)
fn cca_ctrl_init<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    check_mac_en(bus, MacSel::Cmac)?;
    rmw32(
        bus,
        R_AX_CCA_CONTROL,
        B_AX_TB_CHK_TX_NAV
            | B_AX_TB_CHK_CCA_S80
            | B_AX_TB_CHK_CCA_S40
            | B_AX_TB_CHK_CCA_S20
            | B_AX_SIFS_CHK_CCA_S80
            | B_AX_SIFS_CHK_CCA_S40
            | B_AX_SIFS_CHK_CCA_S20
            | B_AX_CTN_CHK_TXNAV
            | B_AX_SIFS_CHK_EDCCA,
        B_AX_TB_CHK_BASIC_NAV
            | B_AX_TB_CHK_BTCCA
            | B_AX_TB_CHK_EDCCA
            | B_AX_TB_CHK_CCA_P20
            | B_AX_SIFS_CHK_BTCCA
            | B_AX_SIFS_CHK_CCA_P20
            | B_AX_CTN_CHK_INTRA_NAV
            | B_AX_CTN_CHK_BASIC_NAV
            | B_AX_CTN_CHK_BTCCA
            | B_AX_CTN_CHK_EDCCA
            | B_AX_CTN_CHK_CCA_S80
            | B_AX_CTN_CHK_CCA_S40
            | B_AX_CTN_CHK_CCA_S20
            | B_AX_CTN_CHK_CCA_P20,
    )?;
    bus.clr32(
        R_AX_RSP_CHK_SIG,
        B_AX_RSP_CHK_TXNAV | B_AX_RSP_CHK_INTRA_NAV | B_AX_RSP_CHK_BASIC_NAV,
    )?;
    bus.clr32(
        R_AX_TRXPTCL_RESP_0,
        B_AX_RSP_CHK_SEC_CCA_80
            | B_AX_RSP_CHK_SEC_CCA_40
            | B_AX_RSP_CHK_SEC_CCA_20
            | B_AX_RSP_CHK_BTCCA
            | B_AX_RSP_CHK_EDCCA
            | B_AX_RSP_CHK_CCA,
    )?;
    Ok(())
}

// Linux: nav_ctrl_init_ax (mac.c:2794), no check_mac_en there
fn nav_ctrl_init<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    bus.set32(
        R_AX_WMAC_NAV_CTL,
        B_AX_WMAC_PLCP_UP_NAV_EN | B_AX_WMAC_TF_UP_NAV_EN | B_AX_WMAC_NAV_UPPER_EN,
    )?;
    bus.mask32(R_AX_WMAC_NAV_CTL, B_AX_WMAC_NAV_UPPER_MASK, NAV_25MS)?;
    Ok(())
}

// Linux: spatial_reuse_init_ax (mac.c:2808)
fn spatial_reuse_init<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    check_mac_en(bus, MacSel::Cmac)?;
    bus.clr(Width::W8, R_AX_RX_SR_CTRL, B_AX_SR_EN)?;
    bus.set(Width::W8, R_AX_BSSID_SRC_CTRL, B_AX_PLCP_SRC_EN)?;
    Ok(())
}

// Linux: tmac_init_ax (mac.c:2830)
fn tmac_init<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    check_mac_en(bus, MacSel::Cmac)?;
    bus.clr32(R_AX_MAC_LOOPBACK, B_AX_MACLBK_EN)?;
    bus.mask32(R_AX_TCR0, B_AX_TCR_UDF_THSD_MASK, TCR_UDF_THSD)?;
    bus.mask32(
        R_AX_TXD_FIFO_CTRL,
        B_AX_TXDFIFO_HIGH_MCS_THRE_MASK,
        TXDFIFO_HIGH_MCS_THRE,
    )?;
    bus.mask32(
        R_AX_TXD_FIFO_CTRL,
        B_AX_TXDFIFO_LOW_MCS_THRE_MASK,
        TXDFIFO_LOW_MCS_THRE,
    )?;
    Ok(())
}

// Linux: trxptcl_init_ax (mac.c:2856), rrsr from rtw8852b_rrsr_cfgs (rtw8852b.c:203):
// ref_rate {R_AX_TRXPTCL_RRSR_CTL_0, B_AX_WMAC_RESP_REF_RATE_SEL, 0}, rsc {.., B_AX_WMAC_RESP_RSC_MASK, 2}
fn trxptcl_init<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    check_mac_en(bus, MacSel::Cmac)?;
    let (cck, ofdm) = (B_AX_WMAC_SPEC_SIFS_CCK_MASK, B_AX_WMAC_SPEC_SIFS_OFDM_MASK);
    // 8852B: WMAC_SPEC_SIFS_OFDM_52B
    rmw32(
        bus,
        R_AX_TRXPTCL_RESP_0,
        cck | ofdm,
        prep(cck, WMAC_SPEC_SIFS_CCK) | prep(ofdm, WMAC_SPEC_SIFS_OFDM_52B),
    )?;
    bus.set32(R_AX_RXTRIG_TEST_USER_2, B_AX_RXTRIG_FCSCHK_EN)?;
    bus.mask32(R_AX_TRXPTCL_RRSR_CTL_0, B_AX_WMAC_RESP_REF_RATE_SEL, 0)?;
    bus.mask32(R_AX_TRXPTCL_RRSR_CTL_0, B_AX_WMAC_RESP_RSC_MASK, 2)?;
    Ok(())
}

/// mac.c:2921-2925, the #defines of rmac_init_ax
const TRXCFG_RMAC_CCA_TO: u32 = 32;
const TRXCFG_RMAC_DATA_TO: u32 = 15;
const RX_MAX_LEN_UNIT: u32 = 512;
const PLD_RLS_MAX_PG: u32 = 127;
const RX_SPEC_MAX_LEN: u32 = 11454 + RX_MAX_LEN_UNIT;
/// rst_bacam's read_poll_timeout_atomic: 1000 us
const BACAM_RST_WAIT_US: u32 = 1000;

// Linux: rmac_init_ax's RX_MPDU_MAX_LEN (mac.c:2955-2963) from the DLE quota: min(the
// channel's RX quota, PLD_RLS_MAX_PG) pages, at most RX_SPEC_MAX_LEN, in 512-byte units
pub fn rx_max_len(c0_rx_qta: u32, ple_pg_size: u32) -> u32 {
    let qta = c0_rx_qta.min(PLD_RLS_MAX_PG);
    qta.saturating_mul(ple_pg_size).min(RX_SPEC_MAX_LEN) / RX_MAX_LEN_UNIT
}

// Linux: rst_bacam (mac.c:2904): a warn-only poll ("failed to reset BA CAM")
fn rst_bacam<B: Bus + ?Sized>(bus: &mut B, m: &mut MacState) -> Rtw89Result<()> {
    bus.mask32(
        R_AX_RESPBA_CAM_CTRL,
        B_AX_BACAM_RST_MASK,
        S_AX_BACAM_RST_ALL,
    )?;
    if let Some(value) = poll_warn(
        bus,
        R_AX_RESPBA_CAM_CTRL,
        B_AX_BACAM_RST_MASK,
        0,
        BACAM_RST_WAIT_US,
    )? {
        m.push(MacEvent::BacamResetTimeout {
            reg: R_AX_RESPBA_CAM_CTRL,
            value,
        });
    }
    Ok(())
}

// Linux: rmac_init_ax (mac.c:2919) with rst_bacam
fn rmac_init<B: Bus + ?Sized>(bus: &mut B, m: &mut MacState) -> Rtw89Result<()> {
    check_mac_en(bus, MacSel::Cmac)?;
    rst_bacam(bus, m)?;
    bus.set(Width::W8, R_AX_RESPBA_CAM_CTRL, B_AX_SSN_SEL)?;
    let (dt, ct) = (B_AX_RX_DLK_DATA_TIME_MASK, B_AX_RX_DLK_CCA_TIME_MASK);
    // 8852B: no RX_DLK_RST_EN (8852BT only)
    rmw16(
        bus,
        R_AX_DLK_PROTECT_CTL,
        dt | ct,
        prep(dt, TRXCFG_RMAC_DATA_TO) | prep(ct, TRXCFG_RMAC_CCA_TO),
    )?;
    bus.mask(Width::W8, R_AX_RCR, B_AX_CH_EN_MASK, 0x1)?;
    let info = m.dmac.dle_info.ok_or(Rtw89Error::Invalid {
        what: "rmac init before the DLE init (no RX quota)",
        value: 0,
    })?;
    let len = rx_max_len(info.c0_rx_qta, info.ple_pg_size);
    bus.mask32(R_AX_RX_FLTR_OPT, B_AX_RX_MPDU_MAX_LEN_MASK, len)?;
    // 8852A CBV patch: not this chip
    bus.clr(Width::W8, R_AX_PLCP_HDR_FLTR, B_AX_VHT_SU_SIGB_CRC_CHK)?;
    Ok(())
}

// Linux: cmac_com_init_ax (mac.c:2983)
fn cmac_com_init<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    check_mac_en(bus, MacSel::Cmac)?;
    bus.clr32(
        R_AX_TX_SUB_CARRIER_VALUE,
        B_AX_TXSC_20M_MASK | B_AX_TXSC_40M_MASK | B_AX_TXSC_80M_MASK,
    )?;
    bus.mask32(R_AX_PTCL_RRSR1, B_AX_RRSR_RATE_EN_MASK, RRSR_OFDM_CCK_EN)?; // 885xB
    Ok(())
}

// Linux: ptcl_init_ax (mac.c:3021), PCIe, MAC 0
fn ptcl_init<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    check_mac_en(bus, MacSel::Cmac)?;
    let (th, tww) = (
        B_AX_HW_CTS2SELF_PKT_LEN_TH_MASK,
        B_AX_HW_CTS2SELF_PKT_LEN_TH_TWW_MASK,
    );
    rmw32(
        bus,
        R_AX_SIFS_SETTING,
        th | tww,
        prep(th, S_AX_CTS2S_TH_1K) | prep(tww, S_AX_CTS2S_TH_SEC_256B) | B_AX_HW_CTS2SELF_EN,
    )?;
    let to = B_AX_PTCL_TX_ARB_TO_THR_MASK;
    rmw32(
        bus,
        R_AX_PTCL_FSM_MON,
        to | B_AX_PTCL_TX_ARB_TO_MODE,
        prep(to, S_AX_PTCL_TO_2MS),
    )?;
    bus.set(
        Width::W8,
        R_AX_PTCL_COMMON_SETTING_0,
        B_AX_CMAC_TX_MODE_0 | B_AX_CMAC_TX_MODE_1,
    )?;
    bus.clr(
        Width::W8,
        R_AX_PTCL_COMMON_SETTING_0,
        B_AX_PTCL_TRIGGER_SS_EN_0 | B_AX_PTCL_TRIGGER_SS_EN_1 | B_AX_PTCL_TRIGGER_SS_EN_UL,
    )?;
    bus.mask(
        Width::W8,
        R_AX_PTCLRPT_FULL_HDL,
        B_AX_SPE_RPT_PATH_MASK,
        FWD_TO_WLCPU,
    )?;
    bus.mask32(R_AX_AGG_LEN_VHT_0, B_AX_AMPDU_MAX_LEN_VHT_MASK, 0x3FF80)?; // 885xB
    Ok(())
}

// Linux: cmac_dma_init_ax (mac.c:3075), 885xB only
fn cmac_dma_init<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    check_mac_en(bus, MacSel::Cmac)?;
    bus.clr(Width::W8, R_AX_RXDMA_CTRL_0, RX_FULL_MODE)?;
    Ok(())
}

// Linux: cmac_init_ax (mac.c:3093) for MAC 0
pub fn cmac_init<B: Bus + ?Sized>(bus: &mut B, m: &mut MacState) -> Rtw89Result<()> {
    scheduler_init(bus)?;
    addr_cam_init(bus)?;
    rx_fltr_init(bus, m.rx_fltr)?;
    cca_ctrl_init(bus)?;
    nav_ctrl_init(bus)?;
    spatial_reuse_init(bus)?;
    tmac_init(bus)?;
    trxptcl_init(bus)?;
    rmac_init(bus, m)?;
    cmac_com_init(bus)?;
    ptcl_init(bus)?;
    cmac_dma_init(bus)?;
    Ok(())
}

// Linux: trx_init_ax (mac.c:4029); qta SCC is not DBCC, no 8852C patch
pub fn trx_init<B: Bus + ?Sized>(bus: &mut B, m: &mut MacState) -> Rtw89Result<()> {
    dmac_init(bus, m)?;
    cmac_init(bus, m)?;
    imr::enable_imr_dmac(bus)?;
    imr::enable_imr_cmac(bus)?;
    imr::err_imr_ctrl(bus, true)?;
    imr::set_host_rpr(bus)?;
    bus.flush()?;
    Ok(())
}
