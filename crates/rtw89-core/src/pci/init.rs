//! The PCIe host interface's register set-up (JS js/pci.mjs): the pre-init
//! before the firmware download, the post-init of the MAC init, and the
//! core stop's deinit and reset.

use crate::bus::{Bus, prep};
use crate::error::{Rtw89Error, Rtw89Result};
use crate::tables::regs::*;

use super::cfg::{self, PCI_EXP_DEVCTL2, PCI_EXP_DEVCTL2_LTR_EN};
use super::rings::{PciRings, bd_ram};

// Linux: rtw89_pci_ops_mac_pre_init_ax (pci.c:3070), 8852B branches decided.
// Needs the rings (`pci_alloc`): their registers are written in the middle.
pub fn pre_init<B: Bus + ?Sized>(bus: &mut B, rings: &mut PciRings) -> Rtw89Result<()> {
    // rtw89_pci_ber: quirk not set for the 8852BE; rxdma_prefth: 8852A only
    bus.clr32(R_AX_PCIE_PS_CTRL, B_AX_L1OFF_PWR_OFF_EN)?; // l1off_pwroff (pci.c:2669)
    // deglitch_setting, l2_rxen_lat: 8852A/C only
    bus.clr32(R_AX_SYS_PW_CTRL, B_AX_PSUS_OFF_CAPC_EN)?; // aphy_pwrcut (pci.c:2699)
    bus.set32(R_AX_SYS_SDIO_CTRL, B_AX_PCIE_DIS_L2_CTRL_LDO_HCI)?; // hci_ldo (pci.c:2709)
    bus.clr32(R_AX_SYS_SDIO_CTRL, B_AX_PCIE_DIS_WLSUS_AFT_PDN)?;
    // dphy_delay (pci.c:2724), autok_x (pci.c:2388)
    cfg::mdio_mask16(
        bus,
        RAC_REG_REV2,
        BAC_CMU_EN_DLY_MASK,
        PCIE_DPHY_DLY_25US,
        PCIE_PHY_GEN1,
    )?;
    cfg::mdio_mask16(
        bus,
        RAC_REG_FLD_0,
        BAC_AUTOK_N_MASK,
        PCIE_AUTOK_4,
        PCIE_PHY_GEN1,
    )?;
    cfg::auto_refclk_cal(bus)?;
    power_wake(bus, true)?;
    // autoload_hang, l12_vmain, gen2_force_ib, l1_ent_lat, wd_exit_l1: 8852C only
    bus.clr32(R_AX_PCIE_EXP_CTRL, B_AX_SIC_EN_FORCE_CLKREQ)?; // set_sic (pci.c:2786)
    // set_lbc (pci.c:2795), lbc_en on
    let lbc = bus.r32(R_AX_LBC_WATCHDOG)?;
    let lbc = (lbc & !B_AX_LBC_TIMER) | prep(B_AX_LBC_TIMER, MAC_AX_LBC_TMR_2MS);
    let lbc = lbc | B_AX_LBC_FLAG | B_AX_LBC_EN;
    bus.w32(R_AX_LBC_WATCHDOG, lbc)?;
    bus.set32(R_AX_LBC_WATCHDOG, lbc)?;
    // set_io_rcy: 8852C only; set_dbg (pci.c:2841)
    bus.set32(
        R_AX_PCIE_DBG_CTRL,
        B_AX_ASFF_FULL_NO_STK | B_AX_EN_STUCK_DBG,
    )?;
    bus.mask32(
        R_AX_PCIE_EXP_CTRL,
        B_AX_EN_STUCK_DBG | B_AX_ASFF_FULL_NO_STK,
        B_AX_EN_STUCK_DBG,
    )?;
    // set_keep_reg (pci.c:2858)
    bus.set32(
        R_AX_PCIE_INIT_CFG1,
        B_AX_PCIE_TXRST_KEEP_REG | B_AX_PCIE_RXRST_KEEP_REG,
    )?;

    bus.set32(R_AX_PCIE_DMA_STOP1, B_AX_STOP_WPDMA)?;
    ctrl_dma_all(bus, false)?;
    // poll_dma_all_idle (pci.c:2934): poll_txdma_ch_idle_ax (2889), poll_rxdma_ch_idle_ax (2917)
    bus.poll32(R_AX_PCIE_DMA_BUSY1, DMA_BUSY1_CHECK_V1, 0, 1)?;
    bus.poll32(R_AX_PCIE_DMA_BUSY1, B_AX_RXQ_BUSY | B_AX_RPQ_BUSY, 0, 1)?;
    clr_idx_all(bus)?;
    // mode_op (pci.c:2953): BD trunc mode (nothing for the 8852B), RXBD packet mode,
    // bursts, multi tag (read | bit, write: one RMW), tag number, WD intervals
    bus.clr32(R_AX_PCIE_INIT_CFG1, B_AX_RXBD_MODE)?;
    bus.mask32(
        R_AX_PCIE_INIT_CFG1,
        B_AX_PCIE_MAX_TXDMA_MASK,
        MAC_AX_TX_BURST_2048B,
    )?;
    bus.mask32(
        R_AX_PCIE_INIT_CFG1,
        B_AX_PCIE_MAX_RXDMA_MASK,
        MAC_AX_RX_BURST_128B,
    )?;
    bus.set32(R_AX_PCIE_INIT_CFG1, B_AX_LATENCY_CONTROL)?;
    bus.mask32(R_AX_PCIE_EXP_CTRL, B_AX_MAX_TAG_NUM, MAC_AX_TAG_NUM_8)?;
    bus.mask32(
        R_AX_PCIE_INIT_CFG2,
        B_AX_WD_ITVL_IDLE,
        MAC_AX_WD_DMA_INTVL_256NS,
    )?;
    bus.mask32(
        R_AX_PCIE_INIT_CFG2,
        B_AX_WD_ITVL_ACT,
        MAC_AX_WD_DMA_INTVL_256NS,
    )?;
    // 8-byte address info
    bus.set32(
        R_AX_TX_ADDRESS_INFO_MODE_SETTING,
        B_AX_HOST_ADDR_INFO_8B_SEL,
    )?;
    bus.clr32(R_AX_PKTIN_SETTING, B_AX_WD_ADDR_INFO_LENGTH)?;
    reset_trx_rings(bus, rings)?; // rtw89_pci_ops_reset (pci.c:1878)
    // rst_bdram_ax (pci.c:47)
    bus.set32(R_AX_PCIE_INIT_CFG1, B_AX_RST_BDRAM)?;
    bus.poll32(R_AX_PCIE_INIT_CFG1, B_AX_RST_BDRAM, 0, 1)?;
    // only the firmware command channel open for the download:
    // ctrl_txdma_ch_ax(false) (pci.c:249), ctrl_txdma_fw_ch_ax(true) (pci.c:266)
    bus.set32(R_AX_PCIE_DMA_STOP1, B_AX_TX_STOP1_MASK_V1)?;
    bus.clr32(R_AX_PCIE_DMA_STOP1, B_AX_STOP_CH12)?;
    ctrl_dma_all(bus, true)?;
    bus.flush()?;
    Ok(())
}

// Linux: rtw89_pci_power_wake_ax (pci.c:2733)
fn power_wake<B: Bus + ?Sized>(bus: &mut B, up: bool) -> Rtw89Result<()> {
    if up {
        bus.set32(R_AX_HCI_OPT_CTRL, BIT_WAKE_CTRL)?;
    } else {
        bus.clr32(R_AX_HCI_OPT_CTRL, BIT_WAKE_CTRL)?;
    }
    Ok(())
}

// Linux: rtw89_pci_ops_mac_pre_deinit_ax (pci.c:3146), rtw89_hci_mac_pre_deinit
// at the end of rtw89_chip_info_setup: power wake off
pub fn mac_pre_deinit<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    power_wake(bus, false)
}

// Linux: rtw89_pci_reset_trx_rings (pci.c:1776), the register part of
// rtw89_pci_ops_reset: ring sizes, BD RAM, ring addresses (DEF_TXCHADDRS); the
// 8852BE has no group BD address, rx_ring_eq_is_full false, wp_sel_addr 0
// (rtw89_pci_init_wp_16sel writes nothing). The host's indexes go back to 0.
fn reset_trx_rings<B: Bus + ?Sized>(bus: &mut B, rings: &mut PciRings) -> Rtw89Result<()> {
    for ring in rings.tx.iter_mut() {
        ring.wp = 0;
        ring.rp = 0;
        bus.w16(ring.num, ring.len)?;
        let (s, mx, mn) = bd_ram(ring.ch).ok_or(Rtw89Error::Invalid {
            what: "TX channel without BD RAM",
            value: ring.ch.into(),
        })?;
        bus.w32(
            ring.bdram,
            prep(BDRAM_SIDX_MASK, s) | prep(BDRAM_MAX_MASK, mx) | prep(BDRAM_MIN_MASK, mn),
        )?;
        bus.w32(ring.desa_l, ring.bd)?;
        bus.w32(ring.desa_l + 4, 0)?;
    }
    for ring in rings.rx.iter_mut() {
        ring.wp = 0;
        ring.rp = 0;
        ring.idx_stale = false;
        bus.w16(ring.num, ring.len)?;
        bus.w32(ring.desa_l, ring.bd)?;
        bus.w32(ring.desa_l + 4, 0)?;
    }
    Ok(())
}

// Linux: rtw89_pci_clr_idx_all_ax (pci.c:2867); 8852B: no ACH4-7, no CH10/11
fn clr_idx_all<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    bus.set32(
        R_AX_TXBD_RWPTR_CLR1,
        B_AX_CLR_ACH0_IDX
            | B_AX_CLR_ACH1_IDX
            | B_AX_CLR_ACH2_IDX
            | B_AX_CLR_ACH3_IDX
            | B_AX_CLR_CH8_IDX
            | B_AX_CLR_CH9_IDX
            | B_AX_CLR_CH12_IDX,
    )?;
    bus.set32(R_AX_RXBD_RWPTR_CLR, B_AX_CLR_RXQ_IDX | B_AX_CLR_RPQ_IDX)?;
    Ok(())
}

// Linux: rtw89_pci_ctrl_dma_all (pci.c:2114): ctrl_dma_io (2103), then ctrl_dma_trx (2091)
fn ctrl_dma_all<B: Bus + ?Sized>(bus: &mut B, on: bool) -> Rtw89Result<()> {
    let trx = B_AX_RXHCI_EN | B_AX_TXHCI_EN;
    if on {
        bus.clr32(R_AX_PCIE_DMA_STOP1, B_AX_STOP_PCIEIO)?;
        bus.set32(R_AX_PCIE_INIT_CFG1, trx)?;
    } else {
        bus.set32(R_AX_PCIE_DMA_STOP1, B_AX_STOP_PCIEIO)?;
        bus.clr32(R_AX_PCIE_INIT_CFG1, trx)?;
    }
    Ok(())
}

/// rtw89_pci_ltr_is_err_reg_val (pci.h:1731), and 0xDEADBEEF as the JS driver checks too
fn ltr_dead(v: u32) -> bool {
    v == 0xffff_ffff || v == RTW89_R32_EA || v == RTW89_R32_DEAD
}

// Linux: rtw89_pci_ops_mac_post_init_ax (pci.c:3238) with rtw89_pci_ltr_set
// (pci.c:3153) and rtw89_pci_dev_ltr_enabled (pci.c:3042, DEVCTL2 through the
// PCIe capability). Each LTR register is checked as it is read (Linux's order;
// the JS checks the four after one batch: the same trace when they answer).
pub fn post_init<B: Bus + ?Sized>(bus: &mut B, rings: &mut PciRings) -> Rtw89Result<()> {
    let cap = cfg::exp_cap(bus, rings)?;
    let devctl2 = bus.cfg_read(cap + PCI_EXP_DEVCTL2)? & 0xffff;
    if devctl2 & PCI_EXP_DEVCTL2_LTR_EN != 0 {
        for reg in [
            R_AX_LTR_CTRL_0,
            R_AX_LTR_CTRL_1,
            R_AX_LTR_IDLE_LATENCY,
            R_AX_LTR_ACTIVE_LATENCY,
        ] {
            let value = bus.r32(reg)?;
            if ltr_dead(value) {
                return Err(Rtw89Error::LtrDead { reg, value });
            }
        }
        bus.set32(
            R_AX_LTR_CTRL_0,
            B_AX_LTR_HW_EN | B_AX_LTR_EN | B_AX_LTR_WD_NOEMP_CHK,
        )?;
        bus.mask32(R_AX_LTR_CTRL_0, B_AX_LTR_SPACE_IDX_MASK, PCI_LTR_SPC_500US)?;
        bus.mask32(
            R_AX_LTR_CTRL_0,
            B_AX_LTR_IDLE_TIMER_IDX_MASK,
            PCI_LTR_IDLE_TIMER_3_2MS,
        )?;
        bus.mask32(R_AX_LTR_CTRL_1, B_AX_LTR_RX0_TH_MASK, 0x28)?;
        bus.mask32(R_AX_LTR_CTRL_1, B_AX_LTR_RX1_TH_MASK, 0x28)?;
        bus.w32(R_AX_LTR_IDLE_LATENCY, 0x9003_9003)?;
        bus.w32(R_AX_LTR_ACTIVE_LATENCY, 0x880b_880b)?;
    }
    // ADDR info 8-byte mode (8852A and the 885xB)
    bus.set32(
        R_AX_TX_ADDRESS_INFO_MODE_SETTING,
        B_AX_HOST_ADDR_INFO_8B_SEL,
    )?;
    bus.clr32(R_AX_PKTIN_SETTING, B_AX_WD_ADDR_INFO_LENGTH)?;
    bus.clr32(R_AX_PCIE_DMA_STOP1, B_AX_TX_STOP1_MASK_V1)?; // ctrl_txdma_ch_ax(true)
    bus.clr32(R_AX_PCIE_DMA_STOP1, B_AX_STOP_WPDMA | B_AX_STOP_PCIEIO)?; // release PCI IO
    bus.flush()?;
    Ok(())
}

// Linux: rtw89_pci_ops_deinit (pci.c:3053), rtw89_hci_deinit of rtw89_core_stop:
// power wake off, ltr_set(false) (returns at once), DMA io and trx off, the ring
// indexes cleared. The bring-up also runs it as a safety step before the
// power-off of a probe or MAC start that failed (not Linux there).
pub fn deinit<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    power_wake(bus, false)?;
    // 8852B: no LTR software trigger (8852A only)
    ctrl_dma_all(bus, false)?;
    clr_idx_all(bus)?;
    bus.flush()?;
    Ok(())
}

// Linux: rtw89_pci_ops_reset (pci.c:1878), rtw89_hci_reset of rtw89_core_stop
// after the power-off: rtw89_pci_reset_trx_rings; releasing the TX rings and the
// firmware commands is host memory only (nothing is pending when BitsOS polls)
pub fn reset<B: Bus + ?Sized>(bus: &mut B, rings: &mut PciRings) -> Rtw89Result<()> {
    reset_trx_rings(bus, rings)?;
    bus.flush()?;
    Ok(())
}
