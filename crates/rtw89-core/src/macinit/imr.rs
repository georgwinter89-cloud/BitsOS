//! The MAC's internal error masks (they report to the chip's firmware, not to
//! the host: no host interrupt is enabled here) and the release report path, as
//! trx_init_ax sets them (JS js/macinit-imr.mjs). Values from rtw8852b_imr_info
//! (rtw8852b.c:157).

use crate::bus::Bus;
use crate::error::Rtw89Result;
use crate::tables::regs::*;

use super::io::{MacSel, check_mac_en};

// rtw89_write32_clr then rtw89_write32_set of one register (two RMWs, as in the C)
fn clr_set<B: Bus + ?Sized>(bus: &mut B, reg: u32, clr: u32, set: u32) -> Rtw89Result<()> {
    bus.clr32(reg, clr)?;
    bus.set32(reg, set)?;
    Ok(())
}

// Linux: enable_imr_ax (mac.c:3936) for RTW89_DMAC_SEL
pub fn enable_imr_dmac<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    check_mac_en(bus, MacSel::Dmac)?;
    // rtw89_wdrls_imr_enable (mac.c:3739)
    clr_set(
        bus,
        R_AX_WDRLS_ERR_IMR,
        B_AX_WDRLS_IMR_EN_CLR,
        B_AX_WDRLS_IMR_SET,
    )?;
    // rtw89_wsec_imr_enable
    bus.set32(R_AX_SEC_DEBUG, B_AX_IMR_ERROR)?;
    // rtw89_mpdu_trx_imr_enable: 8852B has no extra TX bits (8852C only); its set masks are 0
    bus.clr32(
        R_AX_MPDU_TX_ERR_IMR,
        B_AX_TX_GET_ERRPKTID_INT_EN
            | B_AX_TX_NXT_ERRPKTID_INT_EN
            | B_AX_TX_MPDU_SIZE_ZERO_INT_EN
            | B_AX_TX_OFFSET_ERR_INT_EN
            | B_AX_TX_HDR3_SIZE_ERR_INT_EN,
    )?;
    bus.set32(R_AX_MPDU_TX_ERR_IMR, 0)?;
    bus.clr32(
        R_AX_MPDU_RX_ERR_IMR,
        B_AX_GETPKTID_ERR_INT_EN | B_AX_MHDRLEN_ERR_INT_EN | B_AX_RPT_ERR_INT_EN,
    )?;
    bus.set32(R_AX_MPDU_RX_ERR_IMR, 0)?;
    // rtw89_sta_sch_imr_enable
    clr_set(
        bus,
        R_AX_STA_SCHEDULER_ERR_IMR,
        B_AX_SEARCH_HANG_TIMEOUT_INT_EN
            | B_AX_RPT_HANG_TIMEOUT_INT_EN
            | B_AX_PLE_B_PKTID_ERR_INT_EN,
        B_AX_STA_SCHEDULER_IMR_SET,
    )?;
    // rtw89_txpktctl_imr_enable
    clr_set(
        bus,
        R_AX_TXPKTCTL_ERR_IMR_ISR,
        B_AX_TXPKTCTL_IMR_B0_CLR,
        B_AX_TXPKTCTL_IMR_B0_SET,
    )?;
    clr_set(
        bus,
        R_AX_TXPKTCTL_ERR_IMR_ISR_B1,
        B_AX_TXPKTCTL_IMR_B1_CLR,
        B_AX_TXPKTCTL_IMR_B1_SET,
    )?;
    // rtw89_wde_imr_enable, rtw89_ple_imr_enable
    clr_set(bus, R_AX_WDE_ERR_IMR, B_AX_WDE_IMR_CLR, B_AX_WDE_IMR_SET)?;
    clr_set(bus, R_AX_PLE_ERR_IMR, B_AX_PLE_IMR_CLR, B_AX_PLE_IMR_SET)?;
    // rtw89_pktin_imr_enable
    bus.set32(R_AX_PKTIN_ERR_IMR, B_AX_PKTIN_GETPKTID_ERR_INT_EN)?;
    // rtw89_dispatcher_imr_enable (other_disp_imr_set is 0)
    clr_set(
        bus,
        R_AX_HOST_DISPATCHER_ERR_IMR,
        B_AX_HOST_DISP_IMR_CLR as u32, // a `~(...)` in reg.h: signed in the JS tables
        B_AX_HOST_DISP_IMR_SET,
    )?;
    clr_set(
        bus,
        R_AX_CPU_DISPATCHER_ERR_IMR,
        B_AX_CPU_DISP_IMR_CLR as u32,
        B_AX_CPU_DISP_IMR_SET,
    )?;
    clr_set(
        bus,
        R_AX_OTHER_DISPATCHER_ERR_IMR,
        B_AX_OTHER_DISP_IMR_CLR,
        0,
    )?;
    // rtw89_cpuio_imr_enable
    clr_set(
        bus,
        R_AX_CPUIO_ERR_IMR,
        B_AX_CPUIO_IMR_CLR,
        B_AX_CPUIO_IMR_SET,
    )?;
    // rtw89_bbrpt_imr_enable (bbrpt_err_imr_set is 0)
    bus.set32(
        R_AX_BBRPT_COM_ERR_IMR_ISR,
        B_AX_BBRPT_COM_NULL_PLPKTID_ERR_INT_EN,
    )?;
    clr_set(
        bus,
        R_AX_BBRPT_CHINFO_ERR_IMR_ISR,
        B_AX_BBRPT_CHINFO_IMR_CLR,
        0,
    )?;
    bus.set32(R_AX_BBRPT_DFS_ERR_IMR_ISR, B_AX_BBRPT_DFS_TO_ERR_INT_EN)?;
    bus.set32(R_AX_LA_ERRFLAG, B_AX_LA_IMR_DATA_LOSS_ERR)?;
    Ok(())
}

// Linux: enable_imr_ax (mac.c:3936) for RTW89_CMAC_SEL, MAC 0
pub fn enable_imr_cmac<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    check_mac_en(bus, MacSel::Cmac)?;
    // rtw89_scheduler_imr_enable
    clr_set(
        bus,
        R_AX_SCHEDULE_ERR_IMR,
        B_AX_SORT_NON_IDLE_ERR_INT_EN | B_AX_FSM_TIMEOUT_ERR_INT_EN,
        B_AX_FSM_TIMEOUT_ERR_INT_EN,
    )?;
    // rtw89_ptcl_imr_enable
    clr_set(
        bus,
        R_AX_PTCL_IMR0,
        B_AX_PTCL_IMR_CLR_ALL,
        B_AX_PTCL_IMR_SET,
    )?;
    // rtw89_cdma_imr_enable: 8852B has no second CDMA mask (8852C only)
    clr_set(bus, R_AX_DLE_CTRL, B_AX_DLE_IMR_CLR, B_AX_DLE_IMR_SET)?;
    // rtw89_phy_intf_imr_enable: both masks 0 for 8852B, the two RMWs still happen
    clr_set(bus, R_AX_PHYINFO_ERR_IMR, 0, 0)?;
    // rtw89_rmac_imr_enable, rtw89_tmac_imr_enable
    clr_set(bus, R_AX_RMAC_ERR_ISR, B_AX_RMAC_IMR_CLR, B_AX_RMAC_IMR_SET)?;
    clr_set(
        bus,
        R_AX_TMAC_ERR_IMR_ISR,
        B_AX_TMAC_IMR_CLR,
        B_AX_TMAC_IMR_SET,
    )?;
    Ok(())
}

// Linux: err_imr_ctrl_ax (mac.c:3974); no CMAC1 mask on 885xB
pub fn err_imr_ctrl<B: Bus + ?Sized>(bus: &mut B, en: bool) -> Rtw89Result<()> {
    let (dmac, cmac) = if en {
        (DMAC_ERR_IMR_EN, CMAC0_ERR_IMR_EN)
    } else {
        (DMAC_ERR_IMR_DIS, CMAC0_ERR_IMR_DIS)
    };
    bus.w32(R_AX_DMAC_ERR_IMR, dmac)?;
    bus.w32(R_AX_CMAC_ERR_IMR, cmac)?;
    Ok(())
}

// Linux: set_host_rpr_ax (mac.c:4009), PCIe: release reports by POH
pub fn set_host_rpr<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    bus.mask32(R_AX_WDRLS_CFG, B_AX_WDRLS_MODE_MASK, RTW89_RPR_MODE_POH)?;
    bus.set32(R_AX_RLSRPT0_CFG0, B_AX_RLSRPT0_FLTR_MAP_MASK)?;
    bus.mask32(R_AX_RLSRPT0_CFG1, B_AX_RLSRPT0_AGGNUM_MASK, 30)?;
    bus.mask32(R_AX_RLSRPT0_CFG1, B_AX_RLSRPT0_TO_MASK, 255)?;
    Ok(())
}
