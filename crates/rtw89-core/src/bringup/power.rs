//! Power on and off of the RTL8852BE MAC, the XTAL SI register file, and the
//! chip cut (JS kit/rtwpwr.mjs, js/macinit.mjs, js/bringup-steps.mjs).

use crate::bus::{Bus, Width, field, prep};
use crate::error::{Rtw89Error, Rtw89Result};
use crate::tables::regs::*;

use super::efuse;

/// What decides the branches of rtw8852b_pwr_on_func: the efuse, unknown at probe
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PwrOpts {
    pub rfe: u8,
    pub efuse_valid: bool,
    pub power_k_valid: bool,
    pub cv: u32,
    /// before RTW89_FLAG_PROBE_DONE: the power-on reads the secure-boot efuse bytes
    pub probe: bool,
}

impl PwrOpts {
    /// rtw89_chip_info_setup's power-on: rtwdev->efuse still zero (rfe_type 0, not valid)
    pub const fn probe(cv: u32) -> PwrOpts {
        PwrOpts {
            rfe: 0,
            efuse_valid: false,
            power_k_valid: false,
            cv,
            probe: true,
        }
    }
}

/// What the power-on found out
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PowerOn {
    /// efuse 0x5EC/0x5ED other than FF/FF (`None`: the dump failed, Linux goes on)
    pub secure_boot: Option<bool>,
}

// Linux: rtw89_mac_write_xtal_si_ax (mac.c:7195). Linux warns when the poll's
// value does not echo offset and data; the warning needs the log (WL-4).
pub fn xtal_write<B: Bus + ?Sized>(
    bus: &mut B,
    offset: u32,
    val: u32,
    mask: u32,
) -> Rtw89Result<()> {
    let v = prep(B_AX_WL_XTAL_SI_ADDR_MASK, offset)
        | prep(B_AX_WL_XTAL_SI_DATA_MASK, val)
        | prep(B_AX_WL_XTAL_SI_BITMASK_MASK, mask)
        | prep(B_AX_WL_XTAL_SI_MODE_MASK, XTAL_SI_NORMAL_WRITE)
        | B_AX_WL_XTAL_SI_CMD_POLL;
    bus.w32(R_AX_WLAN_XTAL_SI_CTRL, v)?;
    bus.poll32(R_AX_WLAN_XTAL_SI_CTRL, B_AX_WL_XTAL_SI_CMD_POLL, 0, 50)?;
    Ok(())
}

// Linux: rtw89_mac_read_xtal_si_ax (mac.c:7225). Linux takes the data from the
// poll's last read; an op poll returns no value, so one read follows (as the
// JS driver and the golden trace have it).
pub fn xtal_read<B: Bus + ?Sized>(bus: &mut B, offset: u32) -> Rtw89Result<u32> {
    let v = prep(B_AX_WL_XTAL_SI_ADDR_MASK, offset)
        | prep(B_AX_WL_XTAL_SI_MODE_MASK, XTAL_SI_NORMAL_READ)
        | B_AX_WL_XTAL_SI_CMD_POLL;
    bus.w32(R_AX_WLAN_XTAL_SI_CTRL, v)?;
    bus.poll32(R_AX_WLAN_XTAL_SI_CTRL, B_AX_WL_XTAL_SI_CMD_POLL, 0, 50)?;
    Ok(field(
        bus.r32(R_AX_WLAN_XTAL_SI_CTRL)?,
        B_AX_WL_XTAL_SI_DATA_MASK,
    ))
}

/// The chip cut (hal.cv) and the analog cut (hal.acv)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChipVer {
    pub cv: u32,
    pub acv: u32,
}

// Linux: rtw89_read_chip_ver (core.c:7124), the 885xB branch (acv through XTAL SI)
pub fn read_chip_ver<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<ChipVer> {
    let cv = bus.r32_mask(R_AX_SYS_CFG1, B_AX_CHIP_VER_MASK)?;
    let acv = field(xtal_read(bus, XTAL_SI_CV)?, XTAL_SI_ACV_MASK);
    Ok(ChipVer { cv, acv })
}

// Linux: rtw8852b_pwr_on_func (rtw8852b.c:385), PCIe, with rtw8852b_pwr_sps_ana
// (rtw8852b.c:356) and rtw8852b_pwr_sps_dig_off (rtw8852b.c:364)
pub fn pwr_on_func<B: Bus + ?Sized>(bus: &mut B, o: &PwrOpts) -> Rtw89Result<()> {
    const PW: u32 = R_AX_SYS_PW_CTRL;
    if o.rfe == 5 {
        bus.w16(R_AX_SPS_ANA_ON_CTRL2, RTL8852B_RFE_05_SPS_ANA)?;
    }
    bus.clr32(PW, B_AX_AFSM_WLSUS_EN | B_AX_AFSM_PCIE_SUS_EN)?;
    bus.set32(PW, B_AX_DIS_WLBT_PDNSUSEN_SOPC)?;
    bus.set32(R_AX_WLLPS_CTRL, B_AX_DIS_WLBT_LPSEN_LOPC)?;
    bus.clr32(PW, B_AX_APDM_HPDN)?;
    bus.clr32(PW, B_AX_APFM_SWLPS)?;
    bus.poll32(PW, B_AX_RDY_SYSPWR, B_AX_RDY_SYSPWR, 20)?; // rtw8852b.c:399, 1000 us steps, 20 ms
    bus.set32(R_AX_AFE_LDO_CTRL, B_AX_AON_OFF_PC_EN)?;
    bus.poll32(
        R_AX_AFE_LDO_CTRL,
        B_AX_AON_OFF_PC_EN,
        B_AX_AON_OFF_PC_EN,
        20,
    )?;
    const D: u32 = R_AX_SPS_DIG_OFF_CTRL0;
    if o.rfe == 5 {
        bus.mask32(D, B_AX_C1_L1_MASK, 0x1)?;
        bus.mask32(D, B_AX_C2_L1_MASK, 0x1)?;
        bus.mask32(D, B_AX_C3_L1_MASK, 0x2)?;
        bus.mask32(D, B_AX_R1_L1_MASK, 0x1)?;
    } else {
        bus.mask32(D, B_AX_C1_L1_MASK, 0x1)?;
        bus.mask32(D, B_AX_C3_L1_MASK, 0x3)?;
    }
    bus.set32(PW, B_AX_EN_WLON)?;
    bus.set32(PW, B_AX_APFN_ONMAC)?;
    bus.poll32(PW, B_AX_APFN_ONMAC, 0, 20)?;
    for set in [true, false, true, false, true] {
        if set {
            bus.set(Width::W8, R_AX_PLATFORM_ENABLE, B_AX_PLATFORM_EN)?;
        } else {
            bus.clr(Width::W8, R_AX_PLATFORM_ENABLE, B_AX_PLATFORM_EN)?;
        }
    }
    bus.clr32(R_AX_SYS_SDIO_CTRL, B_AX_PCIE_CALIB_EN_V1)?;
    bus.set32(R_AX_SYS_ADIE_PAD_PWR_CTRL, B_AX_SYM_PADPDN_WL_PTA_1P3)?;
    const A: u32 = XTAL_SI_ANAPAR_WL;
    xtal_write(bus, A, XTAL_SI_GND_SHDN_WL, XTAL_SI_GND_SHDN_WL)?;
    bus.set32(R_AX_SYS_ADIE_PAD_PWR_CTRL, B_AX_SYM_PADPDN_WL_RFC_1P3)?;
    xtal_write(bus, A, XTAL_SI_SHDN_WL, XTAL_SI_SHDN_WL)?;
    xtal_write(bus, A, XTAL_SI_OFF_WEI, XTAL_SI_OFF_WEI)?;
    xtal_write(bus, A, XTAL_SI_OFF_EI, XTAL_SI_OFF_EI)?;
    xtal_write(bus, A, 0, XTAL_SI_RFC2RF)?;
    xtal_write(bus, A, XTAL_SI_PON_WEI, XTAL_SI_PON_WEI)?;
    xtal_write(bus, A, XTAL_SI_PON_EI, XTAL_SI_PON_EI)?;
    xtal_write(bus, A, 0, XTAL_SI_SRAM2RFC)?;
    xtal_write(bus, XTAL_SI_SRAM_CTRL, 0, XTAL_SI_SRAM_DIS)?;
    xtal_write(bus, XTAL_SI_XTAL_XMD_2, 0, XTAL_SI_LDO_LPS)?;
    xtal_write(bus, XTAL_SI_XTAL_XMD_4, 0, XTAL_SI_LPS_CAP)?;
    bus.set32(R_AX_PMC_DBG_CTRL2, B_AX_SYSON_DIS_PMCR_AX_WRMSK)?;
    bus.set32(R_AX_SYS_ISO_CTRL, B_AX_ISO_EB2CORE)?;
    bus.clr32(R_AX_SYS_ISO_CTRL, B_AX_PWC_EV2EF_B15)?;
    bus.delay_us(1000)?; // fsleep(1000)
    bus.clr32(R_AX_SYS_ISO_CTRL, B_AX_PWC_EV2EF_B14)?;
    bus.clr32(R_AX_PMC_DBG_CTRL2, B_AX_SYSON_DIS_PMCR_AX_WRMSK)?;
    if o.efuse_valid && !o.power_k_valid {
        bus.mask32(R_AX_SPS_DIG_ON_CTRL0, B_AX_VOL_L1_MASK, 0x9)?;
        bus.mask32(R_AX_SPS_DIG_ON_CTRL0, B_AX_VREFPFM_L_MASK, 0xA)?;
        if o.cv == CHIP_CBV {
            bus.set32(R_AX_PMC_DBG_CTRL2, B_AX_SYSON_DIS_PMCR_AX_WRMSK)?;
            bus.mask(Width::W16, R_AX_HCI_LDO_CTRL, B_AX_R_AX_VADJ_MASK, 0xA)?;
            bus.clr32(R_AX_PMC_DBG_CTRL2, B_AX_SYSON_DIS_PMCR_AX_WRMSK)?;
        }
    }
    // func_en:
    bus.set32(
        R_AX_DMAC_FUNC_EN,
        B_AX_MAC_FUNC_EN
            | B_AX_DMAC_FUNC_EN
            | B_AX_MPDU_PROC_EN
            | B_AX_WD_RLS_EN
            | B_AX_DLE_WDE_EN
            | B_AX_TXPKT_CTRL_EN
            | B_AX_STA_SCH_EN
            | B_AX_DLE_PLE_EN
            | B_AX_PKT_BUF_EN
            | B_AX_DMAC_TBL_EN
            | B_AX_PKT_IN_EN
            | B_AX_DLE_CPUIO_EN
            | B_AX_DISPATCHER_EN
            | B_AX_BBRPT_EN
            | B_AX_MAC_SEC_EN
            | B_AX_DMACREG_GCKEN,
    )?;
    bus.set32(
        R_AX_CMAC_FUNC_EN,
        B_AX_CMAC_EN
            | B_AX_CMAC_TXEN
            | B_AX_CMAC_RXEN
            | B_AX_FORCE_CMACREG_GCKEN
            | B_AX_PHYINTF_EN
            | B_AX_CMAC_DMA_EN
            | B_AX_PTCLTOP_EN
            | B_AX_SCHEDULER_EN
            | B_AX_TMAC_EN
            | B_AX_RMAC_EN,
    )?;
    bus.mask32(
        R_AX_EECS_EESK_FUNC_SEL,
        B_AX_PINMUX_EESK_FUNC_SEL_MASK,
        PINMUX_EESK_FUNC_SEL_BT_LOG,
    )?;
    Ok(())
}

// Linux: rtw8852b_pwr_off_func (rtw8852b.c:514), PCIe
pub fn pwr_off_func<B: Bus + ?Sized>(bus: &mut B, rfe: u8) -> Rtw89Result<()> {
    if rfe == 5 {
        bus.w16(R_AX_SPS_ANA_ON_CTRL2, RTL8852B_RFE_05_SPS_ANA)?;
    }
    const A: u32 = XTAL_SI_ANAPAR_WL;
    xtal_write(bus, A, XTAL_SI_RFC2RF, XTAL_SI_RFC2RF)?;
    xtal_write(bus, A, 0, XTAL_SI_OFF_EI)?;
    xtal_write(bus, A, 0, XTAL_SI_OFF_WEI)?;
    xtal_write(bus, XTAL_SI_WL_RFC_S0, 0, XTAL_SI_RF00)?;
    xtal_write(bus, XTAL_SI_WL_RFC_S1, 0, XTAL_SI_RF10)?;
    xtal_write(bus, A, XTAL_SI_SRAM2RFC, XTAL_SI_SRAM2RFC)?;
    xtal_write(bus, A, 0, XTAL_SI_PON_EI)?;
    xtal_write(bus, A, 0, XTAL_SI_PON_WEI)?;
    const PW: u32 = R_AX_SYS_PW_CTRL;
    bus.set32(PW, B_AX_EN_WLON)?;
    bus.clr32(R_AX_WLRF_CTRL, B_AX_AFC_AFEDIG)?;
    bus.clr(
        Width::W8,
        R_AX_SYS_FUNC_EN,
        B_AX_FEN_BB_GLB_RSTN | B_AX_FEN_BBRSTB,
    )?;
    bus.clr32(R_AX_SYS_ADIE_PAD_PWR_CTRL, B_AX_SYM_PADPDN_WL_RFC_1P3)?;
    xtal_write(bus, A, 0, XTAL_SI_SHDN_WL)?;
    bus.clr32(R_AX_SYS_ADIE_PAD_PWR_CTRL, B_AX_SYM_PADPDN_WL_PTA_1P3)?;
    xtal_write(bus, A, 0, XTAL_SI_GND_SHDN_WL)?;
    bus.set32(PW, B_AX_APFM_OFFMAC)?;
    bus.poll32(PW, B_AX_APFM_OFFMAC, 0, 20)?;
    bus.w32(R_AX_WLLPS_CTRL, SW_LPS_OPTION)?;
    bus.set32(R_AX_SYS_SWR_CTRL1, B_AX_SYM_CTRL_SPS_PWMFREQ)?;
    bus.mask32(R_AX_SPS_DIG_ON_CTRL0, B_AX_REG_ZCDC_H_MASK, 0x3)?;
    bus.set32(PW, B_AX_APFM_SWLPS)?;
    Ok(())
}

// Linux: rtw89_mac_power_switch (mac.c:1521), PCIe; no boot mode (USB only), no
// leave-PS (BitsOS never enters it). At probe the efuse reads of a power-on
// before RTW89_FLAG_PROBE_DONE: ecv, thermal_k and pwr_data are NULL on AX, the
// secure-boot dump's failure is ignored as in Linux (mac.c:1560).
pub fn power_switch<B: Bus + ?Sized>(
    bus: &mut B,
    on: Option<&PwrOpts>,
    rfe_off: u8,
) -> Rtw89Result<PowerOn> {
    let mut found = PowerOn::default();
    if let Some(o) = on {
        // rtw89_mac_reset_pwr_state_ax (mac.c:4104): MAC already on is -EBUSY for PCIe
        let ste = bus.r32_mask(R_AX_IC_PWR_STATE, B_AX_WLMAC_PWR_STE_MASK)?;
        if ste == MAC_AX_MAC_ON {
            return Err(Rtw89Error::MacAlreadyOn { ic_pwr_state: ste });
        }
        pwr_on_func(bus, o)?;
        if o.probe {
            bus.flush()?;
            found.secure_boot = efuse::read_fw_secure(bus, o.cv)?;
        }
        // rtw89_mac_update_scoreboard (mac.c:1506): btc_sb.n[0].cfg is R_AX_SCOREBOARD; no AON interrupt on AX
        bus.w8(R_AX_SCOREBOARD + 3, MAC_AX_NOTIFY_TP_MAJOR)?;
    } else {
        pwr_off_func(bus, rfe_off)?;
        bus.w8(R_AX_SCOREBOARD + 3, MAC_AX_NOTIFY_PWR_MAJOR)?;
    }
    bus.flush()?;
    Ok(found)
}

// Linux: rtw89_mac_pwr_on (mac.c:1588) without a driver state to keep: macinit's
// `mac_pwr_on` (off once and on again after a chip failure) on a scratch state; the
// bring-up keeps the state and the events in `Bringup::mac`
pub fn mac_pwr_on<B: Bus + ?Sized>(bus: &mut B, o: &PwrOpts) -> Rtw89Result<PowerOn> {
    crate::macinit::mac_pwr_on(
        bus,
        o,
        &mut crate::macinit::MacState::new(),
        &mut crate::fw::FwState::new(),
    )
}

// Linux: rtw89_mac_pwr_off (mac.c:1603) without a driver state (the safety step's); the
// bring-up uses macinit's `mac_pwr_off`, which clears the flags as mac.c:1572-1578
pub fn mac_pwr_off<B: Bus + ?Sized>(bus: &mut B, rfe: u8) -> Rtw89Result<()> {
    power_switch(bus, None, rfe).map(|_| ())
}
