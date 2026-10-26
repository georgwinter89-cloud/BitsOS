//! The MAC's DMA pre-init of the RTL8852BE (JS js/dmac.mjs): DLE (the chip's
//! packet memory) quotas and HCI flow control, as Linux rtw89 mac.c does for an
//! AX chip on PCIe. Tables: `tables::dmac` (mac.c rtw89_mac_size, rtw8852b.c).

use alloc::vec::Vec;

use crate::bus::{Bus, field, prep};
use crate::error::{Rtw89Error, Rtw89Result};
use crate::tables::dmac::{
    DLE_MEM, DMA_CH_MASK, DMA_H2C, FIFO, HFC_CH, HFC_PREC, HFC_PUB, SCC_RSVD,
};
use crate::tables::regs::*;
use crate::tables::{DleMem, HfcCh, HfcPrec, HfcPub};

/// enum rtw89_qta_mode, the modes the 8852BE on PCIe uses
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QtaMode {
    /// normal operation (rtwdev->mac.qta_mode)
    #[default]
    Scc,
    /// during the firmware download
    Dlfw,
}

impl QtaMode {
    /// The row name in rtw8852b_dle_mem_pcie (`tables::dmac::DLE_MEM`)
    pub const fn name(self) -> &'static str {
        match self {
            QtaMode::Scc => "SCC",
            QtaMode::Dlfw => "DLFW",
        }
    }
}

/// struct rtw89_dle_info
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DleInfo {
    pub qta_mode: QtaMode,
    pub ple_pg_size: u32,
    pub ple_free_pg: u32,
    pub c0_rx_qta: u32,
    pub c1_rx_qta: u32,
}

/// struct rtw89_hfc_ch_info
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChInfo {
    pub aval: u32,
    pub used: u32,
}

/// struct rtw89_hfc_pub_info
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PubInfo {
    pub g0_used: u32,
    pub g1_used: u32,
    pub g0_aval: u32,
    pub g1_aval: u32,
    pub pub_aval: u32,
    pub wp_aval: u32,
}

/// struct rtw89_hfc_param (rtwdev->mac.hfc_param); empty until the first reset
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HfcParam {
    pub en: bool,
    pub h2c_en: bool,
    pub mode: u32,
    pub prec: Option<HfcPrec>,
    pub pub_cfg: Option<HfcPub>,
    pub ch: Option<&'static [HfcCh]>,
    /// (DMA channel, info) of the channels hfc_upd_ch_info read, in channel order
    pub ch_info: Vec<(u32, ChInfo)>,
    pub pub_info: PubInfo,
}

/// The JS driver's `d.mac` as far as the DMA pre-init fills it (struct rtw89_mac_info)
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DmacState {
    pub qta_mode: QtaMode,
    pub dle_info: Option<DleInfo>,
    pub hfc: HfcParam,
}

/// Which MAC part rtw89_mac_check_mac_en_ax checks (band 0)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacSel {
    Dmac,
    Cmac,
}

// Linux: rtw89_mac_check_mac_en_ax (mac.c:62), the test of the value read
// (-EFAULT): RTW89_R32_EA / _DEAD or a needed bit off
pub fn mac_en_check(value: u32, sel: MacSel) -> Rtw89Result<()> {
    let (reg, need, name) = match sel {
        MacSel::Dmac => (
            R_AX_DMAC_FUNC_EN,
            B_AX_MAC_FUNC_EN | B_AX_DMAC_FUNC_EN,
            "dmac",
        ),
        MacSel::Cmac => (R_AX_CMAC_FUNC_EN, B_AX_CMAC_EN, "cmac"),
    };
    if value == RTW89_R32_EA || value == RTW89_R32_DEAD || value & need != need {
        return Err(Rtw89Error::MacNotEnabled {
            sel: name,
            reg,
            value,
        });
    }
    Ok(())
}

// Linux: rtw89_mac_check_mac_en_ax (mac.c:62), band 0: one read, then the test
pub fn check_mac_en<B: Bus + ?Sized>(bus: &mut B, sel: MacSel) -> Rtw89Result<()> {
    let reg = match sel {
        MacSel::Dmac => R_AX_DMAC_FUNC_EN,
        MacSel::Cmac => R_AX_CMAC_FUNC_EN,
    };
    let v = bus.r32(reg)?;
    mac_en_check(v, sel)
}

// Linux: rtw89_mac_ctrl_hci_dma_trx (mac.h:1642)
pub fn hci_dma_trx<B: Bus + ?Sized>(bus: &mut B, on: bool) -> Rtw89Result<()> {
    let b = B_AX_HCI_TXDMA_EN | B_AX_HCI_RXDMA_EN;
    if on {
        bus.set32(R_AX_HCI_FUNC_EN, b)?;
    } else {
        bus.clr32(R_AX_HCI_FUNC_EN, b)?;
    }
    Ok(())
}

// Linux: get_dle_mem_cfg (mac.c:1906) on rtw8852b_dle_mem_pcie (`rows`)
fn dle_mem(rows: &[DleMem], mode: QtaMode) -> Rtw89Result<&DleMem> {
    rows.iter()
        .find(|m| m.mode == mode.name())
        .ok_or(Rtw89Error::Invalid {
            what: "no DLE memory row for this quota mode",
            value: mode as u32,
        })
}

/// One quota of a generated row; a short row is an error, never a panic
fn qt(row: &[u16], i: usize) -> Rtw89Result<u32> {
    row.get(i)
        .map(|&v| u32::from(v))
        .ok_or(Rtw89Error::Invalid {
            what: "quota row too short",
            value: i as u32,
        })
}

/// u32_replace_bits
const fn replace(v: u32, mask: u32, x: u32) -> u32 {
    (v & !mask) | prep(mask, x)
}

/// mac.h:601-615: the DLE page sizes and their PKTBUF_CFG selectors
const WDE_PG_128: u32 = 128;
const WDE_PG_256: u32 = 256;
const PLE_PG_128: u32 = 128;
const PLE_PG_256: u32 = 256;
const S_AX_WDE_PAGE_SEL_128: u32 = 1; // Linux: mac.h:606
const S_AX_PLE_PAGE_SEL_256: u32 = 2; // Linux: mac.h:615

// Linux: rtw89_mac_dle_init (mac.c:2274) with dle_func_en_ax (2069),
// dle_clk_en_ax (2079), dle_mix_cfg_ax (2092), dle_quota_cfg (2264):
// wde_quota_cfg_ax (2178), ple_quota_cfg_ax (2193), chk_dle_rdy_ax (2149).
// A refused configuration (no row for the mode, the size check, a page size
// dle_mix_cfg_ax does not take) goes Linux's `error:` way: the DLE is switched
// off and both INI_STATUS registers are read into the error (`DleConfig`).
// On the 8852BE's constant tables none of these can happen (the JS writes
// nothing then).
pub fn dle_init<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut DmacState,
    mode: QtaMode,
    ext_mode: QtaMode,
) -> Rtw89Result<()> {
    dle_init_rows(bus, st, mode, ext_mode, DLE_MEM)
}

/// `dle_init` on the given DLE memory rows (the tests' bad rows; `DLE_MEM` otherwise)
pub(crate) fn dle_init_rows<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut DmacState,
    mode: QtaMode,
    ext_mode: QtaMode,
    rows: &[DleMem],
) -> Rtw89Result<()> {
    check_mac_en(bus, MacSel::Dmac)?;
    match dle_cfg(bus, st, mode, ext_mode, rows) {
        Err(Rtw89Error::Invalid { what, value }) => {
            // error: dle_func_en(false), then the two status registers for the message
            bus.clr32(R_AX_DMAC_FUNC_EN, B_AX_DLE_WDE_EN | B_AX_DLE_PLE_EN)?;
            let wde_ini_status = bus.r32(R_AX_WDE_INI_STATUS)?;
            let ple_ini_status = bus.r32(R_AX_PLE_INI_STATUS)?;
            return Err(Rtw89Error::DleConfig {
                what,
                value,
                wde_ini_status,
                ple_ini_status,
            });
        }
        r => r?,
    };
    bus.set32(R_AX_DMAC_FUNC_EN, B_AX_DLE_WDE_EN | B_AX_DLE_PLE_EN)?; // dle_func_en(true)
    // chk_dle_rdy_ax: read_poll_timeout 2000 us
    bus.poll32(R_AX_WDE_INI_STATUS, WDE_MGN_INI_RDY, WDE_MGN_INI_RDY, 2)?;
    bus.poll32(R_AX_PLE_INI_STATUS, PLE_MGN_INI_RDY, PLE_MGN_INI_RDY, 2)?;
    bus.flush()?;
    Ok(())
}

// rtw89_mac_dle_init from get_dle_mem_cfg up to dle_quota_cfg; `Invalid` is
// what Linux sends to its `error:` label
fn dle_cfg<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut DmacState,
    mode: QtaMode,
    ext_mode: QtaMode,
    rows: &[DleMem],
) -> Rtw89Result<()> {
    let cfg = dle_mem(rows, mode)?;
    let ext_wcpu = if mode == QtaMode::Dlfw {
        Some(qt(dle_mem(rows, ext_mode)?.wde_min, 1)?)
    } else {
        None
    };
    let (w, p) = (cfg.wde_size, cfg.ple_size);
    let (w_pg, w_lnk, w_unl) = (
        u32::from(w.pge_size),
        u32::from(w.lnk_pge_num),
        u32::from(w.unlnk_pge_num),
    );
    let (p_pg, p_lnk, p_unl) = (
        u32::from(p.pge_size),
        u32::from(p.lnk_pge_num),
        u32::from(p.unlnk_pge_num),
    );
    // u64: the products of two u16 table values overflow u32 (never on these tables)
    let wide = |pg: u32, a: u32, b: u32| u64::from(pg) * (u64::from(a) + u64::from(b));
    let used = wide(w_pg, w_lnk, w_unl) + wide(p_pg, p_lnk, p_unl);
    // dle_expected_used_size (mac.c:2058)
    let expected = if mode == QtaMode::Scc {
        FIFO - SCC_RSVD
    } else {
        FIFO
    };
    if used != u64::from(expected) {
        return Err(Rtw89Error::Invalid {
            what: "wd/dle mem cfg",
            value: u32::try_from(used).unwrap_or(u32::MAX),
        });
    }
    st.dle_info = Some(DleInfo {
        qta_mode: mode,
        ple_pg_size: p_pg,
        ple_free_pg: p_lnk,
        c0_rx_qta: qt(cfg.ple_min, 6)?,
        c1_rx_qta: qt(cfg.ple_min, 7)?,
    });

    bus.clr32(R_AX_DMAC_FUNC_EN, B_AX_DLE_WDE_EN | B_AX_DLE_PLE_EN)?; // dle_func_en(false)
    bus.set32(R_AX_DMAC_CLK_EN, B_AX_DLE_WDE_CLK_EN | B_AX_DLE_PLE_CLK_EN)?; // dle_clk_en(true)

    // dle_mix_cfg_ax: read, replace fields, write (the 8852B's page sizes: WDE 64, PLE 128)
    let v = bus.r32(R_AX_WDE_PKTBUF_CFG)?;
    // Linux's switch: 256 refused, 128, anything else (default) as 64
    let wde_sel = match w_pg {
        WDE_PG_256 => {
            return Err(Rtw89Error::Invalid {
                what: "WDE DLE page size 256 not supported",
                value: w_pg,
            });
        }
        WDE_PG_128 => S_AX_WDE_PAGE_SEL_128,
        _ => S_AX_WDE_PAGE_SEL_64,
    };
    let v = replace(v, B_AX_WDE_PAGE_SEL_MASK, wde_sel);
    let v = replace(v, B_AX_WDE_START_BOUND_MASK, 0);
    let v = replace(v, B_AX_WDE_FREE_PAGE_NUM_MASK, w_lnk);
    bus.w32(R_AX_WDE_PKTBUF_CFG, v)?;
    let v = bus.r32(R_AX_PLE_PKTBUF_CFG)?;
    // Linux keeps it in a u8; the field takes what fits
    let bound = (wide(w_pg, w_lnk, w_unl) / u64::from(DLE_BOUND_UNIT)) as u32;
    // Linux's switch: 128, 256, anything else (default = 64) refused
    let ple_sel = match p_pg {
        PLE_PG_128 => S_AX_PLE_PAGE_SEL_128,
        PLE_PG_256 => S_AX_PLE_PAGE_SEL_256,
        _ => {
            return Err(Rtw89Error::Invalid {
                what: "PLE DLE page size not supported",
                value: p_pg,
            });
        }
    };
    let v = replace(v, B_AX_PLE_PAGE_SEL_MASK, ple_sel);
    let v = replace(v, B_AX_PLE_START_BOUND_MASK, bound);
    let v = replace(v, B_AX_PLE_FREE_PAGE_NUM_MASK, p_lnk);
    bus.w32(R_AX_PLE_PKTBUF_CFG, v)?;

    // dle_quota_cfg: wde_quota_cfg_ax (hif, wcpu, pkt_in, cpu_io), ple_quota_cfg_ax (11 of them)
    let wq = |i: usize| -> Rtw89Result<u32> {
        Ok(prep(B_AX_WDE_MIN_SIZE_MASK, qt(cfg.wde_min, i)?)
            | prep(B_AX_WDE_MAX_SIZE_MASK, qt(cfg.wde_max, i)?))
    };
    bus.w32(R_AX_WDE_QTA0_CFG, wq(0)?)?;
    let wcpu_min = match ext_wcpu {
        Some(v) => v,
        None => qt(cfg.wde_min, 1)?,
    };
    bus.w32(
        R_AX_WDE_QTA1_CFG,
        prep(B_AX_WDE_MIN_SIZE_MASK, wcpu_min) | prep(B_AX_WDE_MAX_SIZE_MASK, qt(cfg.wde_max, 1)?),
    )?;
    bus.w32(R_AX_WDE_QTA3_CFG, wq(2)?)?;
    bus.w32(R_AX_WDE_QTA4_CFG, wq(3)?)?;
    let ple_regs = [
        R_AX_PLE_QTA0_CFG,
        R_AX_PLE_QTA1_CFG,
        R_AX_PLE_QTA2_CFG,
        R_AX_PLE_QTA3_CFG,
        R_AX_PLE_QTA4_CFG,
        R_AX_PLE_QTA5_CFG,
        R_AX_PLE_QTA6_CFG,
        R_AX_PLE_QTA7_CFG,
        R_AX_PLE_QTA8_CFG,
        R_AX_PLE_QTA9_CFG,
        R_AX_PLE_QTA10_CFG,
    ];
    for (i, reg) in ple_regs.into_iter().enumerate() {
        bus.w32(
            reg,
            prep(B_AX_PLE_MIN_SIZE_MASK, qt(cfg.ple_min, i)?)
                | prep(B_AX_PLE_MAX_SIZE_MASK, qt(cfg.ple_max, i)?),
        )?;
    }
    Ok(())
}

// Linux: hfc_func_en_ax (mac.c:1178): read, change two bits, write
fn hfc_func_en<B: Bus + ?Sized>(
    bus: &mut B,
    p: &mut HfcParam,
    en: bool,
    h2c_en: bool,
) -> Rtw89Result<()> {
    let v = bus.r32(R_AX_HCI_FC_CTRL)?;
    p.en = en;
    p.h2c_en = h2c_en;
    let v = if en {
        v | B_AX_HCI_FC_EN
    } else {
        v & !B_AX_HCI_FC_EN
    };
    let v = if h2c_en {
        v | B_AX_HCI_FC_CH12_EN
    } else {
        v & !B_AX_HCI_FC_CH12_EN
    };
    bus.w32(R_AX_HCI_FC_CTRL, v)?;
    Ok(())
}

/// core.h:5092 grp_num: the two public page groups
const HFC_GRP_NUM: u32 = 2;

// Linux: hfc_ch_cfg_chk (mac.c:926), -EINVAL before the channel's write
fn hfc_ch_cfg_chk(ch: u32, c: &HfcCh, prec: &HfcPrec, pub_cfg: &HfcPub) -> Rtw89Result<()> {
    let (min, max, grp) = (u32::from(c.min), u32::from(c.max), u32::from(c.grp));
    let bad = |what, value| Err(Rtw89Error::HfcConfig { what, ch, value });
    if min != 0 && min < u32::from(prec.ch011_prec) {
        return bad("hfc: channel min pages below ch011_prec", min);
    }
    if max > u32::from(pub_cfg.pub_max) {
        return bad("hfc: channel max pages above pub_max", max);
    }
    if grp >= HFC_GRP_NUM {
        return bad("hfc: channel group out of range", grp);
    }
    Ok(())
}

// Linux: hfc_pub_cfg_chk (mac.c:961), -EFAULT before the public page writes
fn hfc_pub_cfg_chk(cfg: &HfcPub) -> Rtw89Result<()> {
    let sum = u32::from(cfg.grp0) + u32::from(cfg.grp1);
    if sum != u32::from(cfg.pub_max) {
        return Err(Rtw89Error::HfcConfig {
            what: "hfc: grp0 + grp1 is not pub_max",
            ch: DMA_H2C,
            value: sum,
        });
    }
    Ok(())
}

fn missing(what: &'static str) -> Rtw89Error {
    Rtw89Error::Invalid { what, value: 0 }
}

// Linux: rtw89_mac_hfc_init (mac.c:1194) with hfc_reset_param (895),
// hfc_func_en_ax (1178), hfc_h2c_cfg_ax (1127), hfc_ch_ctrl (972),
// hfc_pub_ctrl (1027), hfc_mix_cfg_ax (1143), hfc_upd_ch_info (1000) and
// hfc_upd_mix_info (1108) with hfc_get_mix_info_ax (1053). The DMAC check of
// every hfc_ch_ctrl / hfc_pub_ctrl / upd step sits at Linux's place. As the JS,
// the mix info read back updates only en/h2c_en and the public page check
// (hfc_pub_info_chk, mac.c:945), which fails only with flow control on.
pub fn hfc_init<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut DmacState,
    reset: bool,
    en: bool,
    h2c_en: bool,
) -> Rtw89Result<()> {
    let p = &mut st.hfc;
    if reset {
        // rtw8852b_hfc_param_ini_pcie: SCC brings channel and public config, DLFW only prec
        let scc = st.dle_info.map(|i| i.qta_mode) == Some(QtaMode::Scc);
        p.en = false;
        p.mode = RTW89_HCIFC_POH;
        p.prec = Some(HFC_PREC);
        if scc {
            p.pub_cfg = Some(HFC_PUB);
            p.ch = Some(HFC_CH);
        }
    }
    let prec = p.prec.ok_or(missing("hfc: not reset yet"))?;
    check_mac_en(bus, MacSel::Dmac)?;
    hfc_func_en(bus, p, false, false)?;
    if !en && h2c_en {
        // hfc_h2c_cfg_ax
        bus.w32(
            R_AX_CH_PAGE_CTRL,
            prep(B_AX_PREC_PAGE_CH12_MASK, prec.h2c_prec.into()),
        )?;
        bus.mask32(
            R_AX_HCI_FC_CTRL,
            B_AX_HCI_FC_CH12_FULL_COND_MASK,
            prec.h2c_full.into(),
        )?;
        hfc_func_en(bus, p, en, h2c_en)?;
        return Ok(());
    }
    let ch_cfg =
        p.ch.ok_or(missing("hfc: no channel config for this quota mode"))?;
    let pub_cfg = p
        .pub_cfg
        .ok_or(missing("hfc: no public page config for this quota mode"))?;
    let used = |ch: u32| DMA_CH_MASK & (1 << ch) == 0;
    // hfc_ch_ctrl per channel ACH0..B1HI the chip has
    for ch in (0..DMA_H2C).filter(|&c| used(c)) {
        check_mac_en(bus, MacSel::Dmac)?;
        let c = ch_cfg
            .get(ch as usize)
            .ok_or(missing("hfc: channel config too short"))?;
        hfc_ch_cfg_chk(ch, c, &prec, &pub_cfg)?;
        let grp = if c.grp != 0 { B_AX_GRP } else { 0 };
        bus.w32(
            R_AX_ACH0_PAGE_CTRL + ch * 4,
            prep(B_AX_MIN_PG_MASK, c.min.into()) | prep(B_AX_MAX_PG_MASK, c.max.into()) | grp,
        )?;
    }
    // hfc_pub_ctrl
    check_mac_en(bus, MacSel::Dmac)?;
    hfc_pub_cfg_chk(&pub_cfg)?;
    bus.w32(
        R_AX_PUB_PAGE_CTRL1,
        prep(B_AX_PUBPG_G0_MASK, pub_cfg.grp0.into())
            | prep(B_AX_PUBPG_G1_MASK, pub_cfg.grp1.into()),
    )?;
    bus.w32(
        R_AX_WP_PAGE_CTRL2,
        prep(B_AX_WP_THRD_MASK, pub_cfg.wp_thrd.into()),
    )?;
    // hfc_mix_cfg_ax
    bus.w32(
        R_AX_CH_PAGE_CTRL,
        prep(B_AX_PREC_PAGE_CH011_MASK, prec.ch011_prec.into())
            | prep(B_AX_PREC_PAGE_CH12_MASK, prec.h2c_prec.into()),
    )?;
    bus.w32(
        R_AX_PUB_PAGE_CTRL2,
        prep(B_AX_PUBPG_ALL_MASK, pub_cfg.pub_max.into()),
    )?;
    bus.w32(
        R_AX_WP_PAGE_CTRL1,
        prep(B_AX_PREC_PAGE_WP_CH07_MASK, prec.wp_ch07_prec.into())
            | prep(B_AX_PREC_PAGE_WP_CH811_MASK, prec.wp_ch811_prec.into()),
    )?;
    let v = bus.r32(R_AX_HCI_FC_CTRL)?;
    let v = replace(v, B_AX_HCI_FC_MODE_MASK, p.mode);
    let v = replace(v, B_AX_HCI_FC_WD_FULL_COND_MASK, prec.ch011_full.into());
    let v = replace(v, B_AX_HCI_FC_CH12_FULL_COND_MASK, prec.h2c_full.into());
    let v = replace(
        v,
        B_AX_HCI_FC_WP_CH07_FULL_COND_MASK,
        prec.wp_ch07_full.into(),
    );
    let v = replace(
        v,
        B_AX_HCI_FC_WP_CH811_FULL_COND_MASK,
        prec.wp_ch811_full.into(),
    );
    bus.w32(R_AX_HCI_FC_CTRL, v)?;
    if en || h2c_en {
        hfc_func_en(bus, p, en, h2c_en)?;
        bus.delay_us(10)?;
    }
    // hfc_upd_ch_info for ACH0..B1HI (not CH12)
    p.ch_info.clear();
    for ch in (0..DMA_H2C).filter(|&c| used(c)) {
        check_mac_en(bus, MacSel::Dmac)?;
        let v = bus.r32(R_AX_ACH0_PAGE_INFO + ch * 4)?;
        p.ch_info.push((
            ch,
            ChInfo {
                aval: field(v, B_AX_AVAL_PG_MASK),
                used: field(v, B_AX_USE_PG_MASK),
            },
        ));
    }
    // hfc_upd_mix_info: hfc_get_mix_info_ax's reads in its order
    check_mac_en(bus, MacSel::Dmac)?;
    let info1 = bus.r32(R_AX_PUB_PAGE_INFO1)?;
    let info3 = bus.r32(R_AX_PUB_PAGE_INFO3)?;
    let info2 = bus.r32(R_AX_PUB_PAGE_INFO2)?;
    let wp_info1 = bus.r32(R_AX_WP_PAGE_INFO1)?;
    let fc = bus.r32(R_AX_HCI_FC_CTRL)?;
    bus.r32(R_AX_CH_PAGE_CTRL)?;
    let pub_all = bus.r32(R_AX_PUB_PAGE_CTRL2)?;
    bus.r32(R_AX_WP_PAGE_CTRL1)?;
    bus.r32(R_AX_WP_PAGE_CTRL2)?;
    bus.r32(R_AX_PUB_PAGE_CTRL1)?;
    p.pub_info = PubInfo {
        g0_used: field(info1, B_AX_G0_USE_PG_MASK),
        g1_used: field(info1, B_AX_G1_USE_PG_MASK),
        g0_aval: field(info3, B_AX_G0_AVAL_PG_MASK),
        g1_aval: field(info3, B_AX_G1_AVAL_PG_MASK),
        pub_aval: field(info2, B_AX_PUB_AVAL_PG_MASK),
        wp_aval: field(wp_info1, B_AX_WP_AVAL_PG_MASK),
    };
    p.en = fc & B_AX_HCI_FC_EN != 0;
    p.h2c_en = fc & B_AX_HCI_FC_CH12_EN != 0;
    let pub_max = field(pub_all, B_AX_PUBPG_ALL_MASK);
    let pi = p.pub_info;
    if p.en && pi.g0_used + pi.g1_used + pi.pub_aval != pub_max {
        return Err(Rtw89Error::HfcPubPages {
            g0_used: pi.g0_used,
            g1_used: pi.g1_used,
            pub_aval: pi.pub_aval,
            pub_max,
        });
    }
    Ok(())
}

// Linux: rtw89_mac_dmac_pre_init (mac.c:4258) with rtw89_mac_hci_func_en_ax
// (4208) and rtw89_mac_dmac_func_pre_en_ax (4222; 8852B: DISPATCHER_CLK only)
pub fn dmac_pre_init<B: Bus + ?Sized>(bus: &mut B, st: &mut DmacState) -> Rtw89Result<()> {
    bus.w32(
        R_AX_DMAC_FUNC_EN,
        B_AX_MAC_FUNC_EN | B_AX_DMAC_FUNC_EN | B_AX_DISPATCHER_EN | B_AX_PKT_BUF_EN,
    )?;
    bus.w32(R_AX_DMAC_CLK_EN, B_AX_DISPATCHER_CLK_EN)?;
    let ext = st.qta_mode;
    dle_init(bus, st, QtaMode::Dlfw, ext)?;
    hfc_init(bus, st, true, false, true)
}
