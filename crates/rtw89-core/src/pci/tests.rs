//! js/test-pci.mjs in Rust: the pci and dmac modules on the in-memory bus, op
//! by op against lists written from pci.c (line numbers at each block), not
//! from the Rust code; plus the failure paths (no node needed). The same
//! scenarios also run against the JS modules themselves (tests/pci_js.rs).

use alloc::vec;
use alloc::vec::Vec;

use super::rings::RING_LEN_MAX;
use super::*;
use crate::bringup::{Bringup, RingConfig};
use crate::bus::{Bus, BusError, field, lowbit, prep};
use crate::error::Rtw89Error;
use crate::membus::MemBus;
use crate::tables::regs::*;
use crate::trace::{Op, OpKind, TraceEntry};

// ops as MemBus records them (kit/wlkit.mjs Ops)
fn w(n: u8, a: u32, v: u32) -> Op {
    let m = match n {
        1 => 0xff,
        2 => 0xffff,
        _ => u32::MAX,
    };
    Op::new(OpKind::Wr, n, 0, a, 0, v & m)
}
fn rd(n: u8, a: u32) -> Op {
    Op::new(OpKind::Rd, n, 0, a, 0, 0)
}
fn set(n: u8, a: u32, bits: u32) -> Op {
    Op::new(
        OpKind::Rmw,
        n,
        0,
        a,
        0,
        if n == 2 { bits & 0xffff } else { bits },
    )
}
fn clr(n: u8, a: u32, bits: u32) -> Op {
    Op::new(
        OpKind::Rmw,
        n,
        0,
        a,
        if n == 2 { bits & 0xffff } else { bits },
        0,
    )
}
/// rtw89_write32_mask
fn mask(a: u32, m: u32, v: u32) -> Op {
    Op::new(OpKind::Rmw, 4, 0, a, m, prep(m, v))
}
fn poll(n: u8, a: u32, m: u32, v: u32, ms: u16) -> Op {
    Op::new(OpKind::Poll, n, ms, a, m, v)
}
fn cfgr(off: u32) -> Op {
    Op::new(OpKind::Cfgr, 4, 0, off, 0, 0)
}
fn cfgw(off: u32, v: u32) -> Op {
    Op::new(OpKind::Cfgw, 4, 0, off, 0, v)
}
fn mw(a: u32, v: u32) -> Op {
    Op::new(OpKind::Mwr, 4, 0, a, 0, v)
}
fn delay(us: u32) -> Op {
    Op::new(OpKind::Delay, 0, 0, us, 0, 0)
}

fn ops(b: &MemBus, from: usize) -> Vec<Op> {
    b.trace[from..]
        .iter()
        .filter_map(|e| match e {
            TraceEntry::Op(o) => Some(*o),
            TraceEntry::Note(_) => None,
        })
        .collect()
}

fn same(got: &[Op], want: &[Op], what: &str) {
    for i in 0..got.len().max(want.len()) {
        assert_eq!(
            got.get(i).map(Op::line),
            want.get(i).map(Op::line),
            "{what}: op {i}"
        );
    }
}

// test-pci.mjs's chip: PCIe capability at 0x70, link at gen 1, LTR on; MDIO done at
// once with 0xffff in RDATA, the L1 bit set in DBI byte 0x719, an LBC watchdog value
const MDIO_RDATA: u32 = 0xffff;
const L1_BYTE: u32 = 0x47;
const LBC: u32 = 0x1234_5678;
const LNK_GEN1: u32 = 0x1011_0000;

fn fake(lnk: u32) -> MemBus {
    let mut b = MemBus::new();
    for (o, v) in [
        (0x04, 0x0010_0002),
        (0x34, 0x70),
        (0x70, 0x0002_0010),
        (0x80, lnk),
        (0x98, 0x400),
    ] {
        b.cfg.insert(o, v);
    }
    b.forced.insert(R_AX_MDIO_CFG, 0);
    b.forced.insert(R_AX_MDIO_RDATA, MDIO_RDATA);
    b.forced
        .insert(R_AX_DBI_RDATA + (RTW89_PCIE_L1_CTRL & 3), L1_BYTE);
    b.forced.insert(R_AX_LBC_WATCHDOG, LBC);
    b
}

// ---- the C helpers as op lists ----

// rtw89_pci_check_mdio (pci.c:2120); MDIO_CFG reads 0
fn check_mdio(addr: u32, speed: u32, rw_bit: u32) -> Vec<Op> {
    let pg = match (speed == PCIE_PHY_GEN1, addr < 0x20) {
        (true, true) => MDIO_PG0_G1,
        (true, false) => MDIO_PG1_G1,
        (false, true) => MDIO_PG0_G2,
        (false, false) => MDIO_PG1_G2,
    };
    vec![
        w(1, R_AX_MDIO_CFG, addr & 0x1f),
        rd(2, R_AX_MDIO_CFG),
        w(2, R_AX_MDIO_CFG, prep(B_AX_MDIO_PHY_ADDR_MASK, pg)),
        set(2, R_AX_MDIO_CFG, rw_bit),
        poll(2, R_AX_MDIO_CFG, rw_bit, 0, 2),
    ]
}
// pci.c:2152
fn read16_mdio(addr: u32, speed: u32) -> Vec<Op> {
    let mut v = check_mdio(addr, speed, B_AX_MDIO_RFLAG);
    v.push(rd(2, R_AX_MDIO_RDATA));
    v
}
// pci.c:2167
fn write16_mdio(addr: u32, data: u32, speed: u32) -> Vec<Op> {
    let mut v = vec![w(2, R_AX_MDIO_WDATA, data)];
    v.extend(check_mdio(addr, speed, B_AX_MDIO_WFLAG));
    v
}
// pci.c:2182
fn write16_mdio_mask(addr: u32, m: u32, data: u32, speed: u32) -> Vec<Op> {
    let val = ((MDIO_RDATA & !m) | ((data << lowbit(m)) & m)) & 0xffff;
    let mut v = read16_mdio(addr, speed);
    v.extend(write16_mdio(addr, val, speed));
    v
}
// rtw89_dbi_read8 (pci.c:2256) / rtw89_dbi_write8 (pci.c:2233)
fn dbi_read8(addr: u32) -> Vec<Op> {
    vec![
        w(2, R_AX_DBI_FLAG, addr & B_AX_DBI_ADDR_MSK),
        w(1, R_AX_DBI_FLAG + 2, B_AX_DBI_RFLAG >> 16),
        poll(1, R_AX_DBI_FLAG + 2, 0xff, 0, 1),
        rd(1, R_AX_DBI_RDATA + (addr & 3)),
    ]
}
fn dbi_write8(addr: u32, data: u32) -> Vec<Op> {
    let lsb = addr & B_AX_DBI_2LSB;
    let wa = (addr & B_AX_DBI_ADDR_MSK) | prep(B_AX_DBI_WREN_MSK, 1 << lsb);
    vec![
        w(1, R_AX_DBI_WDATA + lsb, data),
        w(2, R_AX_DBI_FLAG, wa),
        w(1, R_AX_DBI_FLAG + 2, B_AX_DBI_WFLAG >> 16),
        poll(1, R_AX_DBI_FLAG + 2, 0xff, 0, 1),
    ]
}
// rtw89_pci_auto_refclk_cal (pci.c:2400), autook_en false: the PHY rate byte 0x82 in
// config space (its dword), the extended byte 0x719 through DBI
fn auto_refclk_cal(lnk: u32) -> Vec<Op> {
    let l1 = RTW89_PCIE_L1_CTRL;
    let rate = field(
        lnk >> ((RTW89_PCIE_PHY_RATE & 3) * 8),
        RTW89_PCIE_PHY_RATE_MASK,
    );
    let phy = if rate == 1 {
        PCIE_PHY_GEN1
    } else {
        PCIE_PHY_GEN2
    };
    let mut v = vec![cfgr(RTW89_PCIE_PHY_RATE & !3)];
    v.extend(dbi_read8(l1));
    let l1_flag = L1_BYTE & RTW89_PCIE_BIT_L1 != 0;
    if l1_flag {
        v.extend(dbi_write8(l1, L1_BYTE & !RTW89_PCIE_BIT_L1));
    }
    v.extend(read16_mdio(RAC_CTRL_PPR_V1, phy));
    if MDIO_RDATA & B_AX_CALIB_EN != 0 {
        v.extend(write16_mdio(
            RAC_CTRL_PPR_V1,
            MDIO_RDATA & !B_AX_CALIB_EN & 0xffff,
            phy,
        ));
    }
    if l1_flag {
        v.extend(dbi_write8(l1, L1_BYTE));
    }
    v
}
// rtw89_pci_clr_idx_all_ax (pci.c:2867), 8852B
fn clr_idx_all() -> Vec<Op> {
    let tx = [
        B_AX_CLR_ACH0_IDX,
        B_AX_CLR_ACH1_IDX,
        B_AX_CLR_ACH2_IDX,
        B_AX_CLR_ACH3_IDX,
        B_AX_CLR_CH8_IDX,
        B_AX_CLR_CH9_IDX,
        B_AX_CLR_CH12_IDX,
    ]
    .iter()
    .fold(0, |a, b| a | b);
    vec![
        set(4, R_AX_TXBD_RWPTR_CLR1, tx),
        set(4, R_AX_RXBD_RWPTR_CLR, B_AX_CLR_RXQ_IDX | B_AX_CLR_RPQ_IDX),
    ]
}
// rtw89_pci_ctrl_dma_all (pci.c:2114) = dma_io (2103) + dma_trx (2091)
fn ctrl_dma_all(en: bool) -> Vec<Op> {
    let trx = B_AX_RXHCI_EN | B_AX_TXHCI_EN;
    if en {
        vec![
            clr(4, R_AX_PCIE_DMA_STOP1, B_AX_STOP_PCIEIO),
            set(4, R_AX_PCIE_INIT_CFG1, trx),
        ]
    } else {
        vec![
            set(4, R_AX_PCIE_DMA_STOP1, B_AX_STOP_PCIEIO),
            clr(4, R_AX_PCIE_INIT_CFG1, trx),
        ]
    }
}
// rtw89_pci_reset_trx_rings (pci.c:1776): rtw89_bd_ram_table_single (pci.c:1711), DEF_TXCHADDRS
fn reset_trx_rings(r: &PciRings) -> Vec<Op> {
    let bd_ram = [
        (
            "ACH0",
            0,
            5,
            2,
            R_AX_ACH0_TXBD_NUM,
            R_AX_ACH0_BDRAM_CTRL,
            R_AX_ACH0_TXBD_DESA_L,
        ),
        (
            "ACH1",
            5,
            5,
            2,
            R_AX_ACH1_TXBD_NUM,
            R_AX_ACH1_BDRAM_CTRL,
            R_AX_ACH1_TXBD_DESA_L,
        ),
        (
            "ACH2",
            10,
            5,
            2,
            R_AX_ACH2_TXBD_NUM,
            R_AX_ACH2_BDRAM_CTRL,
            R_AX_ACH2_TXBD_DESA_L,
        ),
        (
            "ACH3",
            15,
            5,
            2,
            R_AX_ACH3_TXBD_NUM,
            R_AX_ACH3_BDRAM_CTRL,
            R_AX_ACH3_TXBD_DESA_L,
        ),
        (
            "CH8",
            20,
            4,
            1,
            R_AX_CH8_TXBD_NUM,
            R_AX_CH8_BDRAM_CTRL,
            R_AX_CH8_TXBD_DESA_L,
        ),
        (
            "CH9",
            24,
            4,
            1,
            R_AX_CH9_TXBD_NUM,
            R_AX_CH9_BDRAM_CTRL,
            R_AX_CH9_TXBD_DESA_L,
        ),
        (
            "CH12",
            28,
            4,
            1,
            R_AX_CH12_TXBD_NUM,
            R_AX_CH12_BDRAM_CTRL,
            R_AX_CH12_TXBD_DESA_L,
        ),
    ];
    let mut v = Vec::new();
    for (ch, s, mx, mn, num, bdram, desa) in bd_ram {
        let ring = r.tx(ch).unwrap();
        v.extend([
            w(2, num, ring.len),
            w(
                4,
                bdram,
                prep(BDRAM_SIDX_MASK, s) | prep(BDRAM_MAX_MASK, mx) | prep(BDRAM_MIN_MASK, mn),
            ),
            w(4, desa, ring.bd),
            w(4, desa + 4, 0),
        ]);
    }
    for (ch, num, desa) in [
        ("RXQ", R_AX_RXQ_RXBD_NUM, R_AX_RXQ_RXBD_DESA_L),
        ("RPQ", R_AX_RPQ_RXBD_NUM, R_AX_RPQ_RXBD_DESA_L),
    ] {
        let ring = r.rx(ch).unwrap();
        v.extend([w(2, num, ring.len), w(4, desa, ring.bd), w(4, desa + 4, 0)]);
    }
    v
}

// rtw89_pci_ops_mac_pre_init_ax (pci.c:3070) for the 8852BE (rtw8852be.c rtw8852b_pci_info)
fn pre_init_ops(r: &PciRings, lnk: u32) -> Vec<Op> {
    let lbc = ((LBC & !B_AX_LBC_TIMER) | prep(B_AX_LBC_TIMER, MAC_AX_LBC_TMR_2MS))
        | B_AX_LBC_FLAG
        | B_AX_LBC_EN;
    let mut v = vec![
        // ber: no quirk; rxdma_prefth: 8852A; l1off_pwroff (2669)
        clr(4, R_AX_PCIE_PS_CTRL, B_AX_L1OFF_PWR_OFF_EN),
        // deglitch_setting, l2_rxen_lat: 8852A/C; aphy_pwrcut (2699), hci_ldo (2709)
        clr(4, R_AX_SYS_PW_CTRL, B_AX_PSUS_OFF_CAPC_EN),
        set(4, R_AX_SYS_SDIO_CTRL, B_AX_PCIE_DIS_L2_CTRL_LDO_HCI),
        clr(4, R_AX_SYS_SDIO_CTRL, B_AX_PCIE_DIS_WLSUS_AFT_PDN),
    ];
    v.extend(write16_mdio_mask(
        RAC_REG_REV2,
        BAC_CMU_EN_DLY_MASK,
        PCIE_DPHY_DLY_25US,
        PCIE_PHY_GEN1,
    )); // dphy_delay (2724)
    v.extend(write16_mdio_mask(
        RAC_REG_FLD_0,
        BAC_AUTOK_N_MASK,
        PCIE_AUTOK_4,
        PCIE_PHY_GEN1,
    )); // autok_x (2388)
    v.extend(auto_refclk_cal(lnk));
    v.extend([
        set(4, R_AX_HCI_OPT_CTRL, BIT_WAKE_CTRL), // power_wake_ax (2733)
        // autoload_hang, l12_vmain, gen2_force_ib, l1_ent_lat, wd_exit_l1: 8852C; set_sic (2786)
        clr(4, R_AX_PCIE_EXP_CTRL, B_AX_SIC_EN_FORCE_CLKREQ),
        rd(4, R_AX_LBC_WATCHDOG), // set_lbc (2795)
        w(4, R_AX_LBC_WATCHDOG, lbc),
        set(4, R_AX_LBC_WATCHDOG, lbc),
        // set_io_rcy: 8852C; set_dbg (2841), set_keep_reg (2858)
        set(
            4,
            R_AX_PCIE_DBG_CTRL,
            B_AX_ASFF_FULL_NO_STK | B_AX_EN_STUCK_DBG,
        ),
        mask(
            R_AX_PCIE_EXP_CTRL,
            B_AX_EN_STUCK_DBG | B_AX_ASFF_FULL_NO_STK,
            B_AX_EN_STUCK_DBG,
        ),
        set(
            4,
            R_AX_PCIE_INIT_CFG1,
            B_AX_PCIE_TXRST_KEEP_REG | B_AX_PCIE_RXRST_KEEP_REG,
        ),
        set(4, R_AX_PCIE_DMA_STOP1, B_AX_STOP_WPDMA),
    ]);
    v.extend(ctrl_dma_all(false));
    v.extend([
        poll(4, R_AX_PCIE_DMA_BUSY1, DMA_BUSY1_CHECK_V1, 0, 1), // poll_dma_all_idle (2934): 100 us
        poll(4, R_AX_PCIE_DMA_BUSY1, B_AX_RXQ_BUSY | B_AX_RPQ_BUSY, 0, 1),
    ]);
    v.extend(clr_idx_all());
    v.extend([
        // mode_op (2953)
        clr(4, R_AX_PCIE_INIT_CFG1, B_AX_RXBD_MODE),
        mask(
            R_AX_PCIE_INIT_CFG1,
            B_AX_PCIE_MAX_TXDMA_MASK,
            MAC_AX_TX_BURST_2048B,
        ),
        mask(
            R_AX_PCIE_INIT_CFG1,
            B_AX_PCIE_MAX_RXDMA_MASK,
            MAC_AX_RX_BURST_128B,
        ),
        set(4, R_AX_PCIE_INIT_CFG1, B_AX_LATENCY_CONTROL),
        mask(R_AX_PCIE_EXP_CTRL, B_AX_MAX_TAG_NUM, MAC_AX_TAG_NUM_8),
        mask(
            R_AX_PCIE_INIT_CFG2,
            B_AX_WD_ITVL_IDLE,
            MAC_AX_WD_DMA_INTVL_256NS,
        ),
        mask(
            R_AX_PCIE_INIT_CFG2,
            B_AX_WD_ITVL_ACT,
            MAC_AX_WD_DMA_INTVL_256NS,
        ),
        set(
            4,
            R_AX_TX_ADDRESS_INFO_MODE_SETTING,
            B_AX_HOST_ADDR_INFO_8B_SEL,
        ),
        clr(4, R_AX_PKTIN_SETTING, B_AX_WD_ADDR_INFO_LENGTH),
    ]);
    v.extend(reset_trx_rings(r)); // ops_reset (1878)
    v.extend([
        set(4, R_AX_PCIE_INIT_CFG1, B_AX_RST_BDRAM), // rst_bdram_ax (47)
        poll(4, R_AX_PCIE_INIT_CFG1, B_AX_RST_BDRAM, 0, 1),
        set(4, R_AX_PCIE_DMA_STOP1, B_AX_TX_STOP1_MASK_V1), // ctrl_txdma_ch_ax(false) (249)
        clr(4, R_AX_PCIE_DMA_STOP1, B_AX_STOP_CH12),        // ctrl_txdma_fw_ch_ax(true) (266)
    ]);
    v.extend(ctrl_dma_all(true));
    v
}

// rtw89_pci_ops_mac_post_init_ax (pci.c:3238) with rtw89_pci_ltr_set (pci.c:3153)
fn post_init_ops() -> Vec<Op> {
    vec![
        cfgr(0x70 + 0x28),
        rd(4, R_AX_LTR_CTRL_0),
        rd(4, R_AX_LTR_CTRL_1),
        rd(4, R_AX_LTR_IDLE_LATENCY),
        rd(4, R_AX_LTR_ACTIVE_LATENCY),
        set(
            4,
            R_AX_LTR_CTRL_0,
            B_AX_LTR_HW_EN | B_AX_LTR_EN | B_AX_LTR_WD_NOEMP_CHK,
        ),
        mask(R_AX_LTR_CTRL_0, B_AX_LTR_SPACE_IDX_MASK, PCI_LTR_SPC_500US),
        mask(
            R_AX_LTR_CTRL_0,
            B_AX_LTR_IDLE_TIMER_IDX_MASK,
            PCI_LTR_IDLE_TIMER_3_2MS,
        ),
        mask(R_AX_LTR_CTRL_1, B_AX_LTR_RX0_TH_MASK, 0x28),
        mask(R_AX_LTR_CTRL_1, B_AX_LTR_RX1_TH_MASK, 0x28),
        w(4, R_AX_LTR_IDLE_LATENCY, 0x9003_9003),
        w(4, R_AX_LTR_ACTIVE_LATENCY, 0x880b_880b),
        set(
            4,
            R_AX_TX_ADDRESS_INFO_MODE_SETTING,
            B_AX_HOST_ADDR_INFO_8B_SEL,
        ),
        clr(4, R_AX_PKTIN_SETTING, B_AX_WD_ADDR_INFO_LENGTH),
        clr(4, R_AX_PCIE_DMA_STOP1, B_AX_TX_STOP1_MASK_V1),
        clr(4, R_AX_PCIE_DMA_STOP1, B_AX_STOP_WPDMA | B_AX_STOP_PCIEIO),
    ]
}

const ISR_RXQ: u32 = B_AX_RXP1DMA_INT | B_AX_RXDMA_INT | B_AX_RDU_INT;

/// test-pci.mjs from top to bottom, on one chip
#[test]
fn test_pci_mjs_op_by_op() {
    let mut d = fake(LNK_GEN1);
    let mut rings = pci_alloc(&mut d, &RingConfig::js()).unwrap();
    let mut st = DmacState::default();
    dmac_pre_init(&mut d, &mut st).unwrap();
    let t0 = d.trace.len();
    pre_init(&mut d, &mut rings).unwrap();
    same(
        &ops(&d, t0),
        &pre_init_ops(&rings, LNK_GEN1),
        "pre-init (rtw89_pci_ops_mac_pre_init_ax)",
    );

    // the same at a 5 GT/s link: the GEN2 MDIO pages
    let mut g2 = fake(0x1012_0000);
    let mut r2 = pci_alloc(&mut g2, &RingConfig::js()).unwrap();
    let s0 = g2.trace.len();
    pre_init(&mut g2, &mut r2).unwrap();
    same(
        &ops(&g2, s0),
        &pre_init_ops(&r2, 0x1012_0000),
        "pre-init at gen 2",
    );

    // the capability walk of the PCI core, then the post-init
    let t0 = d.trace.len();
    assert_eq!(exp_cap(&mut d, &mut rings).unwrap(), 0x70);
    same(
        &ops(&d, t0),
        &[cfgr(0x04), cfgr(0x34), cfgr(0x70)],
        "PCIe capability walk",
    );
    let t0 = d.trace.len();
    post_init(&mut d, &mut rings).unwrap();
    same(&ops(&d, t0), &post_init_ops(), "post-init with ltr_set");

    // basic_cfg -> cpl_timeout_cfg (pci.c:4384): DEVCTL2 |= COMP_TMOUT_DIS; LNKCTL read
    let t0 = d.trace.len();
    let bc = basic_cfg(&mut d, &mut rings).unwrap();
    same(
        &ops(&d, t0),
        &[cfgr(0x98), cfgw(0x98, 0x400 | 0x0010), cfgr(0x80)],
        "basic_cfg",
    );
    assert_eq!(
        bc,
        cfg::BasicCfg {
            devctl2: 0x400,
            lnkctl: 0
        }
    );

    // rtw89_pci_fwcmd_submit (pci.c:1551): BD = length | LS << 16, dma; kick (pci.c:1332)
    let ch12 = rings.tx("CH12").unwrap().clone();
    let payload = [0xabu8; 100];
    let t0 = d.trace.len();
    fwcmd(&mut d, &mut rings, &payload, false, 50).unwrap();
    let all = ops(&d, t0);
    let bd: Vec<Op> = all
        .iter()
        .copied()
        .filter(|o| o.kind == OpKind::Mwr && o.a >= ch12.bd && o.a < ch12.bd + ch12.len * 8)
        .collect();
    same(
        &bd,
        &[
            mw(ch12.bd, (24 + 100) | (RTW89_PCI_TXBD_OPT_LS << 16)),
            mw(ch12.bd + 4, ch12.bufs[0]),
        ],
        "CH12 BD",
    );
    let kick: Vec<Op> = all
        .iter()
        .copied()
        .filter(|o| o.kind != OpKind::Mwr)
        .collect();
    same(
        &kick,
        &[
            w(2, R_AX_CH12_TXBD_IDX, 1),
            poll(
                4,
                R_AX_CH12_TXBD_IDX,
                TXBD_HW_IDX_MASK,
                prep(TXBD_HW_IDX_MASK, 1),
                50,
            ),
        ],
        "CH12 kick, then BitsOS waits for the index",
    );
    let mut wd = [0u8; 24];
    d.mem_read_bytes(ch12.bufs[0], &mut wd).unwrap();
    let dw = |i: usize| u32::from_le_bytes([wd[i], wd[i + 1], wd[i + 2], wd[i + 3]]);
    assert_eq!(field(dw(0), RTW89_TXWD_BODY0_CHANNEL_DMA), RTW89_DMA_H2C);
    assert_eq!(field(dw(8), RTW89_TXWD_BODY2_TXPKT_SIZE), 100);
    let mut pl = [0u8; 100];
    d.mem_read_bytes(ch12.bufs[0] + 24, &mut pl).unwrap();
    assert!(
        pl.iter().all(|&x| x == 0xab),
        "the payload follows the WD body"
    );

    // RXQ (pci.c:4533): ISR clear, rxbd_recalc (pci.c:146), per buffer rxbd_info,
    // the index write-back (pci.c:413)
    let rxq = rings.rx("RXQ").unwrap().clone();
    let sizes = [60usize, 1500, 3000];
    for (i, n) in sizes.iter().enumerate() {
        let info =
            prep(RTW89_PCI_RXBD_WRITE_SIZE, 4 + *n as u32) | RTW89_PCI_RXBD_FS | RTW89_PCI_RXBD_LS;
        let mut b = info.to_le_bytes().to_vec();
        b.extend(core::iter::repeat_n(i as u8 + 1, *n));
        d.mem_write_bytes(rxq.bufs[i], &b).unwrap();
    }
    d.forced.insert(rxq.idx, prep(TXBD_HW_IDX_MASK, 3));
    let t0 = d.trace.len();
    let got = rx_poll(&mut d, &mut rings, "RXQ", 32).unwrap();
    let no_mw: Vec<Op> = ops(&d, t0)
        .into_iter()
        .filter(|o| o.kind != OpKind::Mwr)
        .collect();
    same(
        &no_mw,
        &[
            w(4, R_AX_PCIE_HISR00, ISR_RXQ),
            rd(4, rxq.idx),
            w(2, rxq.idx, 3),
        ],
        "RXQ: ISR clear, index read, index write-back",
    );
    let seen: Vec<(usize, bool, bool, u8)> = got
        .iter()
        .map(|x| (x.data.len(), x.fs, x.ls, x.data[0]))
        .collect();
    assert_eq!(
        seen,
        vec![
            (60, true, true, 1),
            (1500, true, true, 2),
            (3000, true, true, 3)
        ]
    );
    let t0 = d.trace.len();
    assert!(rx_poll(&mut d, &mut rings, "RXQ", 32).unwrap().is_empty());
    same(
        &ops(&d, t0),
        &[w(4, R_AX_PCIE_HISR00, ISR_RXQ), rd(4, rxq.idx)],
        "RXQ without new buffers: no index write (rtw89_pci_poll_rxq_dma returns early)",
    );

    // rtw89_pci_ops_deinit (pci.c:3053): power wake off, ltr_set(false) returns, DMA off, indexes cleared
    let t0 = d.trace.len();
    deinit(&mut d).unwrap();
    let mut want = vec![clr(4, R_AX_HCI_OPT_CTRL, BIT_WAKE_CTRL)];
    want.extend(ctrl_dma_all(false));
    want.extend(clr_idx_all());
    same(&ops(&d, t0), &want, "deinit");
    // rtw89_pci_ops_reset (pci.c:1878): rtw89_pci_reset_trx_rings
    let t0 = d.trace.len();
    reset(&mut d, &mut rings).unwrap();
    same(
        &ops(&d, t0),
        &reset_trx_rings(&rings),
        "reset (reset_trx_rings)",
    );
    assert!(rings.tx.iter().all(|x| x.wp == 0 && x.rp == 0) && rings.rx.iter().all(|x| x.wp == 0));
}

#[test]
fn another_capability_layout_is_walked_and_none_is_an_error() {
    // a list 0x40 (PM) -> 0x50 (MSI) -> 0x90 (PCIe): taken where it is (spec/wlan.md §3)
    let mut b = fake(LNK_GEN1);
    b.cfg.insert(0x34, 0x40);
    b.cfg.insert(0x40, 0x5001);
    b.cfg.insert(0x50, 0x9005);
    b.cfg.insert(0x90, 0x0010);
    let mut r = PciRings::default();
    assert_eq!(exp_cap(&mut b, &mut r).unwrap(), 0x90);
    let t0 = b.trace.len();
    assert_eq!(exp_cap(&mut b, &mut r).unwrap(), 0x90);
    assert_eq!(b.trace.len(), t0, "walked once");

    // test-pci.mjs's other layout: nothing at 0x50, so no PCIe capability; no config write follows
    let mut b = fake(LNK_GEN1);
    b.cfg.insert(0x34, 0x50);
    let mut r = PciRings::default();
    assert!(matches!(
        basic_cfg(&mut b, &mut r),
        Err(Rtw89Error::NoPcieCapability { first: 0x50, .. })
    ));
    assert!(!b.lines().iter().any(|l| l.starts_with("cfgw")));

    // a list that points at itself ends after the TTL, a capability list bit off ends at once
    let mut b = fake(LNK_GEN1);
    b.cfg.insert(0x34, 0x40);
    b.cfg.insert(0x40, 0x4001);
    let mut r = PciRings::default();
    assert!(exp_cap(&mut b, &mut r).is_err());
    assert_eq!(b.trace.len(), 2 + 48, "status, pointer, 48 entries");
    let mut b = fake(LNK_GEN1);
    b.cfg.insert(0x04, 0x0000_0002);
    assert!(exp_cap(&mut b, &mut PciRings::default()).is_err());
    assert_eq!(b.trace.len(), 1);
}

#[test]
fn a_phy_rate_without_mdio_pages_stops_the_pre_init() {
    let mut b = fake(0x1013_0000);
    let mut r = pci_alloc(&mut b, &RingConfig::js()).unwrap();
    assert_eq!(
        pre_init(&mut b, &mut r),
        Err(Rtw89Error::PciePhyRate { value: 3 })
    );
    assert_eq!(
        b.lines().last().unwrap(),
        "cfgr 0x00000080 0x00000000 0x00000000"
    );
}

#[test]
fn ltr_registers_that_do_not_answer_stop_the_post_init_where_linux_does() {
    let mut b = fake(LNK_GEN1);
    b.forced.insert(R_AX_LTR_CTRL_1, 0xEAEA_EAEA);
    let mut r = PciRings::default();
    let t0 = b.trace.len();
    assert_eq!(
        post_init(&mut b, &mut r),
        Err(Rtw89Error::LtrDead {
            reg: R_AX_LTR_CTRL_1,
            value: 0xEAEA_EAEA
        })
    );
    let tail = &ops(&b, t0)[3..];
    same(
        tail,
        &[cfgr(0x98), rd(4, R_AX_LTR_CTRL_0), rd(4, R_AX_LTR_CTRL_1)],
        "LTR reads up to the bad one",
    );
    for v in [u32::MAX, 0xDEAD_BEEF] {
        let mut b = fake(LNK_GEN1);
        b.forced.insert(R_AX_LTR_ACTIVE_LATENCY, v);
        assert!(matches!(
            post_init(&mut b, &mut PciRings::default()),
            Err(Rtw89Error::LtrDead { .. })
        ));
    }
    // LTR off in DEVCTL2: no LTR register is touched
    let mut b = fake(LNK_GEN1);
    b.cfg.insert(0x98, 0);
    post_init(&mut b, &mut PciRings::default()).unwrap();
    assert!(!b.lines().iter().any(|l| l.contains("0x00007")));
}

#[test]
fn rx_indexes_from_the_chip_are_checked() {
    let mut b = fake(LNK_GEN1);
    let mut r = pci_alloc(&mut b, &RingConfig::js()).unwrap();
    let idx = r.rx("RXQ").unwrap().idx;
    for v in [prep(TXBD_HW_IDX_MASK, 64), u32::MAX] {
        b.forced.insert(idx, v);
        assert_eq!(
            rx_poll(&mut b, &mut r, "RXQ", 32),
            Err(Rtw89Error::RingIndex {
                reg: idx,
                value: v,
                len: 64
            })
        );
    }
    // a buffer whose rxbd_info says less than its own 4 bytes: empty data, never a panic
    let buf = r.rx("RXQ").unwrap().bufs[0];
    b.mem_write_bytes(buf, &prep(RTW89_PCI_RXBD_WRITE_SIZE, 2).to_le_bytes())
        .unwrap();
    b.forced.insert(idx, prep(TXBD_HW_IDX_MASK, 1));
    let got = rx_poll(&mut b, &mut r, "RXQ", 32).unwrap();
    assert_eq!((got[0].len, got[0].data.len()), (2, 0));
    // the cleared rxbd_info: a stale buffer never looks new
    assert_eq!(b.mem_read32(buf).unwrap(), 0);
    // budget 0 takes nothing and writes no index
    b.forced.insert(idx, prep(TXBD_HW_IDX_MASK, 5));
    assert!(rx_poll(&mut b, &mut r, "RXQ", 0).unwrap().is_empty());
    assert!(rx_poll(&mut b, &mut r, "NOPE", 1).is_err());
}

#[test]
fn ring_lengths_are_checked_before_any_dma() {
    for bad in [0, RING_LEN_MAX + 1] {
        let mut b = fake(LNK_GEN1);
        let cfg = RingConfig {
            ch12: bad,
            ..RingConfig::js()
        };
        assert!(matches!(
            pci_alloc(&mut b, &cfg),
            Err(Rtw89Error::Invalid { .. })
        ));
        assert!(b.dma_live.is_empty() && b.trace.is_empty());
    }
    // Linux's lengths are accepted (DMA permitting) and every ring gets its length
    let mut b = fake(LNK_GEN1);
    assert!(
        pci_alloc(&mut b, &RingConfig::linux()).is_err(),
        "not in the kit's window"
    );
    let mut b = fake(LNK_GEN1);
    b.dma_next = 0x0100_0000;
    let r = pci_alloc(&mut BigBus(&mut b), &RingConfig::linux()).unwrap();
    assert!(r.tx.iter().all(|x| x.len == 256 && x.bufs.len() == 256));
    assert!(r.rx.iter().all(|x| x.len == 256 && x.bufs.len() == 256));
}

/// MemBus with a DMA window up to 2 GB: the kit's 960 KB cannot hold Linux's ring lengths
struct BigBus<'a>(&'a mut MemBus);
impl Bus for BigBus<'_> {
    fn read(&mut self, w: crate::bus::Width, a: u32) -> crate::bus::BusResult<u32> {
        self.0.read(w, a)
    }
    fn write(&mut self, w: crate::bus::Width, a: u32, v: u32) -> crate::bus::BusResult<()> {
        self.0.write(w, a, v)
    }
    fn rmw(&mut self, w: crate::bus::Width, a: u32, c: u32, s: u32) -> crate::bus::BusResult<()> {
        self.0.rmw(w, a, c, s)
    }
    fn poll(
        &mut self,
        w: crate::bus::Width,
        a: u32,
        m: u32,
        v: u32,
        ms: u16,
    ) -> crate::bus::BusResult<()> {
        self.0.poll(w, a, m, v, ms)
    }
    fn delay_us(&mut self, us: u32) -> crate::bus::BusResult<()> {
        self.0.delay_us(us)
    }
    fn cfg_read(&mut self, o: u16) -> crate::bus::BusResult<u32> {
        self.0.cfg_read(o)
    }
    fn cfg_write(&mut self, o: u16, v: u32) -> crate::bus::BusResult<()> {
        self.0.cfg_write(o, v)
    }
    fn dma_alloc(&mut self, size: usize, align: usize) -> crate::bus::BusResult<u32> {
        crate::membus::bump_alloc(&mut self.0.dma_next, 0x8000_0000, size, align)
            .ok_or(BusError::DmaExhausted { size, align })
    }
    fn dma_free(&mut self, _a: u32, _s: usize) -> crate::bus::BusResult<()> {
        Ok(())
    }
    fn mem_write(&mut self, w: crate::bus::MemWidth, a: u32, v: u64) -> crate::bus::BusResult<()> {
        self.0.mem_write(w, a, v)
    }
    fn mem_read32(&mut self, a: u32) -> crate::bus::BusResult<u32> {
        self.0.mem_read32(a)
    }
    fn mem_write_bytes(&mut self, a: u32, d: &[u8]) -> crate::bus::BusResult<()> {
        self.0.mem_write_bytes(a, d)
    }
    fn mem_read_bytes(&mut self, a: u32, o: &mut [u8]) -> crate::bus::BusResult<()> {
        self.0.mem_read_bytes(a, o)
    }
    fn now_ms(&mut self) -> u64 {
        0
    }
}

#[test]
fn fwcmd_refuses_long_payloads_and_a_timeout_leaves_the_index_ahead() {
    let mut b = fake(LNK_GEN1);
    let mut r = pci_alloc(&mut b, &RingConfig::js()).unwrap();
    let t0 = b.trace.len();
    assert!(matches!(
        fwcmd(&mut b, &mut r, &[0; 2025], false, 50),
        Err(Rtw89Error::Invalid { value: 2025, .. })
    ));
    assert_eq!(b.trace.len(), t0, "nothing written");
    fwcmd(&mut b, &mut r, &[0; 2024], true, 50).unwrap();
    // the chip does not fetch: the poll fails, the host index stays ahead (fw resyncs CH12)
    b.failing_polls.insert(R_AX_CH12_TXBD_IDX);
    let e = fwcmd(&mut b, &mut r, &[1; 8], false, 50).unwrap_err();
    assert!(matches!(
        e,
        Rtw89Error::Bus(BusError::PollTimeout {
            addr: R_AX_CH12_TXBD_IDX,
            ..
        })
    ));
    let ch12 = r.tx("CH12").unwrap();
    assert_eq!((ch12.wp, ch12.rp), (2, 1));
    // the ring wraps
    b.failing_polls.clear();
    for _ in 0..15 {
        fwcmd(&mut b, &mut r, &[2; 8], false, 50).unwrap();
    }
    assert_eq!(r.tx("CH12").unwrap().wp, 1);
}

#[test]
fn fill_txdesc_lays_out_body_and_info_as_the_c_structs() {
    let info = TxDescInfo {
        wp_offset: 0x12,
        ch_dma: 8,
        hdr_llc_len: 0x1f,
        wd_page: true,
        fw_dl: false,
        hw_ssn_sel: 2,
        hw_seq_mode: 1,
        tid_indicate: true,
        qsel: 0x13,
        pkt_size: 0x3fff,
        mac_id: 0x7f,
        seq: 0xabc,
        agg_en: true,
        bk: true,
        wd_info: Some([1, 2, 3, 4, 5, 6]),
    };
    let d = fill_txdesc(&info);
    assert_eq!(d.len(), 48);
    let dw = |i: usize| u32::from_le_bytes([d[i * 4], d[i * 4 + 1], d[i * 4 + 2], d[i * 4 + 3]]);
    assert_eq!(
        dw(0),
        0x1200_0000 | 0x40_0000 | 0x8_0000 | 0xF800 | 0x80 | 0x8 | 0x1
    );
    assert_eq!(dw(1), 0);
    assert_eq!(dw(2), 0x80_0000 | (0x13 << 17) | 0x3fff | (0x7f << 24));
    assert_eq!(dw(3), 0xabc | 0x1000 | 0x2000);
    assert_eq!((dw(4), dw(5)), (0, 0));
    assert_eq!((6..12).map(dw).collect::<Vec<_>>(), vec![1, 2, 3, 4, 5, 6]);
    // too big values are cut to their fields; no WD info: the body only, without the bit
    let d = fill_txdesc(&TxDescInfo {
        pkt_size: 0x1_0000 | 5,
        ..TxDescInfo::default()
    });
    assert_eq!(d.len(), 24);
    assert_eq!(
        field(
            u32::from_le_bytes([d[8], d[9], d[10], d[11]]),
            RTW89_TXWD_BODY2_TXPKT_SIZE
        ),
        5
    );
    assert_eq!(
        u32::from_le_bytes([d[0], d[1], d[2], d[3]]) & RTW89_TXWD_BODY0_WD_INFO_EN,
        0
    );
}

// ---- dmac ----

/// mac.c's DLFW pre-init (rtw89_mac_dmac_pre_init, mac.c:4258) as op list, from the C:
/// hci_func_en, dmac_func_pre_en, dle_init(DLFW, SCC), hfc_init(reset, h2c only)
fn dmac_pre_init_ops() -> Vec<Op> {
    let dmac_ok = B_AX_MAC_FUNC_EN | B_AX_DMAC_FUNC_EN | B_AX_DISPATCHER_EN | B_AX_PKT_BUF_EN;
    let mut v = vec![
        w(4, R_AX_DMAC_FUNC_EN, dmac_ok),
        w(4, R_AX_DMAC_CLK_EN, B_AX_DISPATCHER_CLK_EN),
        rd(4, R_AX_DMAC_FUNC_EN), // check_mac_en
        clr(4, R_AX_DMAC_FUNC_EN, B_AX_DLE_WDE_EN | B_AX_DLE_PLE_EN),
        set(
            4,
            R_AX_DMAC_CLK_EN,
            B_AX_DLE_WDE_CLK_EN | B_AX_DLE_PLE_CLK_EN,
        ),
        rd(4, R_AX_WDE_PKTBUF_CFG),
        // wde page 64 (sel 0), bound 0, 0 linked pages
        w(4, R_AX_WDE_PKTBUF_CFG, 0),
        rd(4, R_AX_PLE_PKTBUF_CFG),
        // ple page 128 (sel 1), bound 1024 * 64 / 8192 = 8, 64 linked pages
        w(
            4,
            R_AX_PLE_PKTBUF_CFG,
            prep(B_AX_PLE_PAGE_SEL_MASK, 1)
                | prep(B_AX_PLE_START_BOUND_MASK, 8)
                | prep(B_AX_PLE_FREE_PAGE_NUM_MASK, 64),
        ),
        w(4, R_AX_WDE_QTA0_CFG, 0),
        // the SCC mode's wcpu minimum (ext_wde_min_qt_wcpu = 48)
        w(4, R_AX_WDE_QTA1_CFG, prep(B_AX_WDE_MIN_SIZE_MASK, 48)),
        w(4, R_AX_WDE_QTA3_CFG, 0),
        w(4, R_AX_WDE_QTA4_CFG, 0),
    ];
    // ple_qt13 = {0, 0, 16, 48, 0, ...}, min = max
    let ple = [0u32, 0, 16, 48, 0, 0, 0, 0, 0, 0, 0];
    let regs = [
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
    for (q, reg) in ple.iter().zip(regs) {
        v.push(w(
            4,
            reg,
            prep(B_AX_PLE_MIN_SIZE_MASK, *q) | prep(B_AX_PLE_MAX_SIZE_MASK, *q),
        ));
    }
    v.extend([
        set(4, R_AX_DMAC_FUNC_EN, B_AX_DLE_WDE_EN | B_AX_DLE_PLE_EN),
        poll(4, R_AX_WDE_INI_STATUS, WDE_MGN_INI_RDY, WDE_MGN_INI_RDY, 2),
        poll(4, R_AX_PLE_INI_STATUS, PLE_MGN_INI_RDY, PLE_MGN_INI_RDY, 2),
        // hfc_init(reset, false, true): check, func_en(false, false), h2c_cfg, func_en(false, true)
        rd(4, R_AX_DMAC_FUNC_EN),
        rd(4, R_AX_HCI_FC_CTRL),
        w(4, R_AX_HCI_FC_CTRL, 0),
        w(4, R_AX_CH_PAGE_CTRL, prep(B_AX_PREC_PAGE_CH12_MASK, 40)),
        mask(R_AX_HCI_FC_CTRL, B_AX_HCI_FC_CH12_FULL_COND_MASK, 0),
        rd(4, R_AX_HCI_FC_CTRL),
        w(4, R_AX_HCI_FC_CTRL, B_AX_HCI_FC_CH12_EN),
    ]);
    v
}

#[test]
fn dmac_pre_init_op_by_op() {
    let mut b = fake(LNK_GEN1);
    let mut st = DmacState::default();
    hci_dma_trx(&mut b, true).unwrap();
    dmac_pre_init(&mut b, &mut st).unwrap();
    let mut want = vec![set(
        4,
        R_AX_HCI_FUNC_EN,
        B_AX_HCI_TXDMA_EN | B_AX_HCI_RXDMA_EN,
    )];
    want.extend(dmac_pre_init_ops());
    same(
        &ops(&b, 0),
        &want,
        "rtw89_mac_ctrl_hci_dma_trx + rtw89_mac_dmac_pre_init",
    );
    let dle = st.dle_info.unwrap();
    assert_eq!(
        (dle.qta_mode, dle.ple_pg_size, dle.ple_free_pg),
        (QtaMode::Dlfw, 128, 64)
    );
    assert!(!st.hfc.en && st.hfc.h2c_en);
    assert!(
        st.hfc.ch.is_none() && st.hfc.pub_cfg.is_none(),
        "DLFW brings no channel config"
    );
}

#[test]
fn hfc_full_init_order_and_its_failures() {
    let mut b = fake(LNK_GEN1);
    let mut st = DmacState::default();
    dmac_pre_init(&mut b, &mut st).unwrap();
    // flow control on in DLFW: Linux's param_ini has no channel config there
    assert!(matches!(
        hfc_init(&mut b, &mut st, true, true, true),
        Err(Rtw89Error::Invalid { .. })
    ));
    dle_init(&mut b, &mut st, QtaMode::Scc, QtaMode::Scc).unwrap();
    // the counters read 0: the public pages do not add up to 446 with flow control on
    let t0 = b.trace.len();
    assert_eq!(
        hfc_init(&mut b, &mut st, true, true, true),
        Err(Rtw89Error::HfcPubPages {
            g0_used: 0,
            g1_used: 0,
            pub_aval: 0,
            pub_max: 446
        })
    );
    let got = ops(&b, t0);
    // check, func_en(false, false), then per used channel (ACH0-3, CH8, CH9) check + page ctrl
    let used = [0u32, 1, 2, 3, 8, 9];
    let mut want = vec![
        rd(4, R_AX_DMAC_FUNC_EN),
        rd(4, R_AX_HCI_FC_CTRL),
        w(4, R_AX_HCI_FC_CTRL, 0),
    ];
    let cfg = [
        (5, 341),
        (5, 341),
        (4, 342),
        (4, 342),
        (0, 0),
        (0, 0),
        (0, 0),
        (0, 0),
        (4, 342),
        (4, 342),
    ];
    for ch in used {
        let (mn, mx) = cfg[ch as usize];
        want.push(rd(4, R_AX_DMAC_FUNC_EN));
        want.push(w(
            4,
            R_AX_ACH0_PAGE_CTRL + ch * 4,
            prep(B_AX_MIN_PG_MASK, mn) | prep(B_AX_MAX_PG_MASK, mx),
        ));
    }
    want.extend([
        rd(4, R_AX_DMAC_FUNC_EN),
        w(4, R_AX_PUB_PAGE_CTRL1, prep(B_AX_PUBPG_G0_MASK, 446)),
        w(4, R_AX_WP_PAGE_CTRL2, 0),
        w(
            4,
            R_AX_CH_PAGE_CTRL,
            prep(B_AX_PREC_PAGE_CH011_MASK, 2) | prep(B_AX_PREC_PAGE_CH12_MASK, 40),
        ),
        w(4, R_AX_PUB_PAGE_CTRL2, prep(B_AX_PUBPG_ALL_MASK, 446)),
        w(4, R_AX_WP_PAGE_CTRL1, 0),
        rd(4, R_AX_HCI_FC_CTRL),
        // mode POH (0), WD full cond 1, the rest 0; FC_EN off after func_en(false, false)
        w(4, R_AX_HCI_FC_CTRL, prep(B_AX_HCI_FC_WD_FULL_COND_MASK, 1)),
        rd(4, R_AX_HCI_FC_CTRL),
        w(
            4,
            R_AX_HCI_FC_CTRL,
            prep(B_AX_HCI_FC_WD_FULL_COND_MASK, 1) | B_AX_HCI_FC_EN | B_AX_HCI_FC_CH12_EN,
        ),
        delay(10),
    ]);
    for ch in used {
        want.push(rd(4, R_AX_DMAC_FUNC_EN));
        want.push(rd(4, R_AX_ACH0_PAGE_INFO + ch * 4));
    }
    want.push(rd(4, R_AX_DMAC_FUNC_EN));
    for reg in [
        R_AX_PUB_PAGE_INFO1,
        R_AX_PUB_PAGE_INFO3,
        R_AX_PUB_PAGE_INFO2,
        R_AX_WP_PAGE_INFO1,
        R_AX_HCI_FC_CTRL,
        R_AX_CH_PAGE_CTRL,
        R_AX_PUB_PAGE_CTRL2,
        R_AX_WP_PAGE_CTRL1,
        R_AX_WP_PAGE_CTRL2,
        R_AX_PUB_PAGE_CTRL1,
    ] {
        want.push(rd(4, reg));
    }
    same(&got, &want, "rtw89_mac_hfc_init(reset, en, h2c_en) in SCC");

    // the same without flow control: the page check is off (hfc_upd_mix_info)
    hfc_init(&mut b, &mut st, true, false, false).unwrap();
    assert!(!st.hfc.en);

    // the DMAC turned off in the middle: the next check stops at Linux's place
    let mut b = fake(LNK_GEN1);
    let mut st = DmacState::default();
    dmac_pre_init(&mut b, &mut st).unwrap();
    dle_init(&mut b, &mut st, QtaMode::Scc, QtaMode::Scc).unwrap();
    b.forced.insert(R_AX_DMAC_FUNC_EN, B_AX_MAC_FUNC_EN);
    let t0 = b.trace.len();
    assert_eq!(
        hfc_init(&mut b, &mut st, true, true, true),
        Err(Rtw89Error::MacNotEnabled {
            sel: "dmac",
            reg: R_AX_DMAC_FUNC_EN,
            value: B_AX_MAC_FUNC_EN
        })
    );
    assert_eq!(b.trace.len(), t0 + 1, "nothing after the failed check");
    // never reset: refused before the chip is touched
    let mut b = fake(LNK_GEN1);
    let t0 = b.trace.len();
    assert!(hfc_init(&mut b, &mut DmacState::default(), false, false, true).is_err());
    assert_eq!(b.trace.len(), t0);
}

#[test]
fn mac_en_check_refuses_the_dead_patterns() {
    let ok = B_AX_MAC_FUNC_EN | B_AX_DMAC_FUNC_EN;
    assert!(dmac::mac_en_check(ok, dmac::MacSel::Dmac).is_ok());
    for v in [RTW89_R32_EA, RTW89_R32_DEAD, B_AX_DMAC_FUNC_EN, 0] {
        assert!(dmac::mac_en_check(v, dmac::MacSel::Dmac).is_err(), "{v:#x}");
    }
    assert!(dmac::mac_en_check(B_AX_CMAC_EN, dmac::MacSel::Cmac).is_ok());
    assert!(dmac::mac_en_check(0, dmac::MacSel::Cmac).is_err());
}

// ---- the bring-up steps ----

/// tests.rs's chip (cv 1, acv 1, efuse ready, MAC off) with a gen 1 link; registers
/// are plain storage as on bringup-fake.mjs's FakeMiniPc
fn chip() -> MemBus {
    let mut b = MemBus::new();
    for (o, v) in [
        (0x04, 0x0010_0002),
        (0x34, 0x70),
        (0x70, 0x0002_0010),
        (0x80, LNK_GEN1),
        (0x98, 0x400),
    ] {
        b.cfg.insert(o, v);
    }
    b.forced.insert(R_AX_SYS_CFG1, prep(B_AX_CHIP_VER_MASK, 1));
    b.forced
        .insert(R_AX_WLAN_XTAL_SI_CTRL, prep(B_AX_WL_XTAL_SI_DATA_MASK, 1));
    b.forced.insert(R_AX_EFUSE_CTRL, B_AX_EF_RDY | 0xff);
    b
}

#[test]
fn partial_init_reaches_the_firmware_download_and_the_stop_turns_dma_off_first() {
    let mut b = chip();
    let mut run = Bringup::new();
    run.probe_power_on(&mut b, &RingConfig::js()).unwrap();
    run.partial_init_to_fwdl(&mut b).unwrap();
    assert_eq!(b.lines().len(), 459, "golden lines 1-459");
    assert_eq!(
        b.lines().last().unwrap(),
        "rmw32 0x00001000 0x00000000 0x00002800"
    );
    let t0 = b.lines().len();
    run.stop(&mut b).unwrap();
    let tail = &b.lines()[t0..];
    assert_eq!(tail[0], "# stop");
    assert_eq!(tail[1], "# not Linux: DMA off before this power-off");
    assert_eq!(
        tail[2], "rmw32 0x00000074 0x00000020 0x00000000",
        "power wake off"
    );
    let off = tail.iter().position(|l| l == "# Linux again").unwrap();
    assert_eq!(off, 2 + 5, "deinit: wake, DMA io/trx off, two index clears");
    assert!(
        tail.iter()
            .any(|l| l == "poll32 0x00000004 0x00000200 0x00000000 20 ms"),
        "then the power-off"
    );
    assert!(!run.pci && b.dma_live.is_empty());
}

#[test]
fn the_stop_after_the_core_runs_linux_deinit_and_reset() {
    let mut b = chip();
    let mut run = Bringup::new();
    run.probe_power_on(&mut b, &RingConfig::js()).unwrap();
    run.partial_init_to_fwdl(&mut b).unwrap();
    run.core = true;
    let t0 = b.lines().len();
    let rings = run.rings.clone().unwrap();
    run.stop(&mut b).unwrap();
    let tail = &b.lines()[t0..];
    assert_eq!(
        tail[1], "rmw32 0x00000074 0x00000020 0x00000000",
        "rtw89_hci_deinit, no not-Linux mark"
    );
    let reset_at = tail.iter().position(|l| l == "# Linux again").unwrap() + 1;
    let want: Vec<_> = reset_trx_rings(&rings).iter().map(Op::line).collect();
    assert_eq!(
        &tail[reset_at..reset_at + want.len()],
        &want[..],
        "rtw89_hci_reset after the power-off"
    );
    // a failed power-off: no reset
    let mut b = chip();
    let mut run = Bringup::new();
    run.probe_power_on(&mut b, &RingConfig::js()).unwrap();
    run.core = true;
    b.failing_polls.insert(0x04);
    let t0 = b.lines().len();
    assert!(run.stop(&mut b).is_err());
    assert!(
        !b.lines()[t0..]
            .iter()
            .any(|l| l.starts_with("wr16 0x00001024"))
    );
}

#[test]
fn partial_init_without_rings_or_with_a_dead_dmac_fails_cleanly() {
    let mut b = chip();
    let mut run = Bringup::new();
    assert!(matches!(
        run.partial_init_to_fwdl(&mut b),
        Err(Rtw89Error::Invalid { .. })
    ));
    assert!(b.trace.is_empty() && !run.pci);

    let mut b = chip();
    let mut run = Bringup::new();
    run.probe_power_on(&mut b, &RingConfig::js()).unwrap();
    b.forced.insert(R_AX_DMAC_FUNC_EN, 0xDEAD_BEEF);
    assert!(matches!(
        run.partial_init_to_fwdl(&mut b),
        Err(Rtw89Error::MacNotEnabled {
            value: 0xDEAD_BEEF,
            ..
        })
    ));
    assert!(run.pci, "the stop must turn the DMA off");
    run.stop(&mut b).unwrap();
    assert!(
        b.lines()
            .iter()
            .any(|l| l == "# not Linux: DMA off before this power-off")
    );
}

// ---- the fixer's round: Linux's error paths and the chip's boundary values ----

#[test]
fn a_failed_mdio_access_still_puts_the_l1_bit_back() {
    // pci.c:2536-2546: goto end, the end label writes bdr_ori back. test-pci.mjs's
    // byte 0x47 has L1 off; here it is on.
    let l1 = RTW89_PCIE_L1_CTRL;
    let bdr = L1_BYTE | RTW89_PCIE_BIT_L1;
    let chip = || {
        let mut b = fake(LNK_GEN1);
        b.forced.insert(R_AX_DBI_RDATA + (l1 & 3), bdr);
        b
    };
    let mut b = chip();
    b.failing_polls.insert(R_AX_MDIO_CFG);
    let t0 = b.trace.len();
    let e = cfg::auto_refclk_cal(&mut b).unwrap_err();
    assert!(matches!(
        e,
        Rtw89Error::Bus(BusError::PollTimeout {
            addr: R_AX_MDIO_CFG,
            ..
        })
    ));
    let mut head = vec![cfgr(RTW89_PCIE_PHY_RATE & !3)];
    head.extend(dbi_read8(l1));
    head.extend(dbi_write8(l1, bdr & !RTW89_PCIE_BIT_L1));
    let mut want = head.clone();
    want.extend(check_mdio(RAC_CTRL_PPR_V1, PCIE_PHY_GEN1, B_AX_MDIO_RFLAG));
    want.extend(dbi_write8(l1, bdr));
    same(&ops(&b, t0), &want, "MDIO read times out, L1 restored");
    // the MDIO write failing (after a good read) also restores it
    let mut b = chip();
    let t0 = b.trace.len();
    let write_poll = head.len()
        + read16_mdio(RAC_CTRL_PPR_V1, PCIE_PHY_GEN1).len()
        + write16_mdio(RAC_CTRL_PPR_V1, 0, PCIE_PHY_GEN1).len()
        - 1;
    b.fail_at_op = Some(t0 + write_poll);
    assert_eq!(
        cfg::auto_refclk_cal(&mut b),
        Err(Rtw89Error::Bus(BusError::Gone))
    );
    let got = ops(&b, t0);
    assert_eq!(got.len(), write_poll + 4);
    same(&got[write_poll..], &dbi_write8(l1, bdr), "L1 restored last");
    // a good run with L1 on: the same restore at the end (Linux's end label)
    let mut b = chip();
    let t0 = b.trace.len();
    cfg::auto_refclk_cal(&mut b).unwrap();
    let got = ops(&b, t0);
    same(&got[got.len() - 4..], &dbi_write8(l1, bdr), "good run");
}

#[test]
fn config_byte_writes_never_clear_status_bits() {
    let mut b = fake(LNK_GEN1);
    // master abort and parity error pending in PCI_STATUS
    b.cfg.insert(0x04, 0xF910_0006);
    let t0 = b.trace.len();
    cfg::cfg_write8(&mut b, 0x04, 0x07).unwrap();
    same(
        &ops(&b, t0),
        &[cfgr(0x04), cfgw(0x04, 0x0000_0007)],
        "command byte: status half written 0",
    );
    // a byte at 0x07 clears exactly its bits (Linux's byte write to PCI_STATUS)
    b.cfg.insert(0x04, 0xF910_0006);
    let t0 = b.trace.len();
    cfg::cfg_write8(&mut b, 0x07, 0x80).unwrap();
    same(
        &ops(&b, t0),
        &[cfgr(0x04), cfgw(0x04, 0x8000_0006)],
        "status byte",
    );
    // other header bytes: read-modify-write of their dword
    b.cfg.insert(0x0C, 0x0000_1008);
    cfg::cfg_write8(&mut b, 0x0D, 0x40).unwrap();
    assert_eq!(b.cfg[&0x0C], 0x0000_4008);
    // the capability area (DEVSTA, LNKSTA, PMCSR) is refused before any access
    for addr in [0x40u16, 0x7A, 0x92, 0xFF] {
        let t0 = b.trace.len();
        assert_eq!(
            cfg::cfg_write8(&mut b, addr, 0),
            Err(Rtw89Error::Invalid {
                what: "config byte write into the capability area (no byte config access)",
                value: addr.into()
            })
        );
        assert_eq!(b.trace.len(), t0);
    }
}

/// One RXQ buffer as the chip leaves it: rxbd_info (length `len`, `tag`), then data
fn rx_fill(b: &mut MemBus, r: &PciRings, slot: usize, len: u32, tag: u32) {
    let buf = r.rx("RXQ").unwrap().bufs[slot];
    let info = prep(RTW89_PCI_RXBD_WRITE_SIZE, len)
        | prep(RTW89_PCI_RXBD_TAG, tag)
        | RTW89_PCI_RXBD_FS
        | RTW89_PCI_RXBD_LS;
    b.mem_write_bytes(buf, &info.to_le_bytes()).unwrap();
    let data: Vec<u8> = (0..len.saturating_sub(4))
        .map(|i| (i ^ tag) as u8)
        .collect();
    b.mem_write_bytes(buf + 4, &data).unwrap();
}

#[test]
fn rx_wraps_resumes_after_the_budget_and_cuts_lengths_to_the_buffer() {
    let mut b = fake(LNK_GEN1);
    let mut r = pci_alloc(&mut b, &RingConfig::js()).unwrap();
    let (idx, len, size) = {
        let q = r.rx("RXQ").unwrap();
        (q.idx, q.len, q.size)
    };
    r.rx_mut("RXQ").unwrap().wp = len - 2;
    // four pending across the end of the ring: 62, 63, 0, 1
    for (i, slot) in [len - 2, len - 1, 0, 1].into_iter().enumerate() {
        rx_fill(&mut b, &r, slot as usize, 100 + i as u32, i as u32);
    }
    b.forced.insert(idx, prep(TXBD_HW_IDX_MASK, 2));
    let t0 = b.trace.len();
    let got = rx_poll(&mut b, &mut r, "RXQ", 2).unwrap();
    assert_eq!(got.iter().map(|x| x.tag).collect::<Vec<_>>(), [0, 1]);
    assert_eq!(r.rx("RXQ").unwrap().wp, 0);
    let o = ops(&b, t0);
    assert_eq!(
        o.last().map(Op::line),
        Some(w(2, idx, 0).line()),
        "index wrapped to 0"
    );
    let got = rx_poll(&mut b, &mut r, "RXQ", 2).unwrap();
    assert_eq!(got.iter().map(|x| x.tag).collect::<Vec<_>>(), [2, 3]);
    assert_eq!(got[1].data.len(), 103 - 4);
    assert_eq!(r.rx("RXQ").unwrap().wp, 2);
    assert!(rx_poll(&mut b, &mut r, "RXQ", 2).unwrap().is_empty());

    // a length larger than the buffer: cut to it, never a read past it
    let buf = r.rx("RXQ").unwrap().bufs[2];
    let info = prep(
        RTW89_PCI_RXBD_WRITE_SIZE,
        RTW89_PCI_RXBD_WRITE_SIZE >> lowbit(RTW89_PCI_RXBD_WRITE_SIZE),
    );
    b.mem_write_bytes(buf, &info.to_le_bytes()).unwrap();
    b.forced.insert(idx, prep(TXBD_HW_IDX_MASK, 3));
    let got = rx_poll(&mut b, &mut r, "RXQ", 8).unwrap();
    assert_eq!((got[0].len, got[0].data.len()), (size, size as usize - 4));
}

#[test]
fn rx_bus_errors_never_drop_a_buffer_already_taken() {
    let mut b = fake(LNK_GEN1);
    let mut r = pci_alloc(&mut b, &RingConfig::js()).unwrap();
    let idx = r.rx("RXQ").unwrap().idx;
    let bufs = r.rx("RXQ").unwrap().bufs.clone();
    for slot in 0..3 {
        rx_fill(&mut b, &r, slot, 60, slot as u32);
    }
    b.forced.insert(idx, prep(TXBD_HW_IDX_MASK, 3));
    // ops: HISR00, index read, clear of slot 0, clear of slot 1 (fails)
    let t0 = b.trace.len();
    b.fail_at_op = Some(t0 + 3);
    let got = rx_poll(&mut b, &mut r, "RXQ", 32).unwrap();
    assert_eq!(got.len(), 1, "slot 0 delivered");
    same(
        &ops(&b, t0),
        &[
            w(
                4,
                R_AX_PCIE_HISR00,
                B_AX_RXP1DMA_INT | B_AX_RXDMA_INT | B_AX_RDU_INT,
            ),
            rd(4, idx),
            mw(bufs[0], 0),
            w(2, idx, 1),
        ],
        "slot 0 given back, slot 1 stays",
    );
    // the next poll starts at slot 1
    let got = rx_poll(&mut b, &mut r, "RXQ", 32).unwrap();
    assert_eq!(got.iter().map(|x| x.tag).collect::<Vec<_>>(), [1, 2]);
    // a fault before anything is taken is the error
    rx_fill(&mut b, &r, 3, 60, 3);
    b.forced.insert(idx, prep(TXBD_HW_IDX_MASK, 4));
    b.fail_at_op = Some(b.trace.len() + 2);
    assert_eq!(
        rx_poll(&mut b, &mut r, "RXQ", 32),
        Err(Rtw89Error::Bus(BusError::Gone))
    );
    assert_eq!(r.rx("RXQ").unwrap().wp, 3);
    // the index write failing: the buffer is delivered, the next poll repeats the write first
    let t0 = b.trace.len();
    b.fail_at_op = Some(t0 + 3);
    let got = rx_poll(&mut b, &mut r, "RXQ", 32).unwrap();
    assert_eq!(got.iter().map(|x| x.tag).collect::<Vec<_>>(), [3]);
    assert!(r.rx("RXQ").unwrap().idx_stale);
    let t0 = b.trace.len();
    assert!(rx_poll(&mut b, &mut r, "RXQ", 32).unwrap().is_empty());
    assert_eq!(
        ops(&b, t0)[0].line(),
        w(2, idx, 4).line(),
        "stale index first"
    );
    assert!(!r.rx("RXQ").unwrap().idx_stale);
}

#[test]
fn ch12_never_runs_into_unfetched_buffers_and_resyncs_to_the_chip() {
    let mut b = fake(LNK_GEN1);
    let mut r = pci_alloc(&mut b, &RingConfig::js()).unwrap();
    let (idx, len) = {
        let c = r.tx("CH12").unwrap();
        (c.idx, c.len)
    };
    b.failing_polls.insert(idx);
    // the chip fetches nothing: len - 1 buffers go out (one BD stays free, pci.c:1222)
    for _ in 0..len - 1 {
        assert!(matches!(
            fwcmd(&mut b, &mut r, &[7; 8], false, 50),
            Err(Rtw89Error::Bus(BusError::PollTimeout { .. }))
        ));
    }
    let t0 = b.trace.len();
    assert_eq!(
        fwcmd(&mut b, &mut r, &[7; 8], false, 50),
        Err(Rtw89Error::RingFull {
            reg: idx,
            wp: len - 1,
            rp: 0
        })
    );
    assert_eq!(b.trace.len(), t0, "nothing written on a full ring");
    // resync: the ring goes to the chip's host index; a chip that does not catch up times out
    assert!(matches!(
        fwcmd_resync(&mut b, &mut r, 50),
        Err(Rtw89Error::Bus(BusError::PollTimeout { .. }))
    ));
    b.failing_polls.clear();
    let t0 = b.trace.len();
    assert_eq!(fwcmd_resync(&mut b, &mut r, 50), Ok(len - 1));
    same(
        &ops(&b, t0),
        &[
            rd(4, idx),
            poll(
                4,
                idx,
                TXBD_HW_IDX_MASK,
                prep(TXBD_HW_IDX_MASK, len - 1),
                50,
            ),
        ],
        "index read, wait for the chip",
    );
    let c = r.tx("CH12").unwrap();
    assert_eq!((c.wp, c.rp), (len - 1, len - 1));
    fwcmd(&mut b, &mut r, &[8; 8], false, 50).unwrap();
    assert_eq!(r.tx("CH12").unwrap().wp, 0);
    // all ones: the link is gone, the ring is left alone
    b.forced.insert(idx, u32::MAX);
    assert_eq!(
        fwcmd_resync(&mut b, &mut r, 50),
        Err(Rtw89Error::RingIndex {
            reg: idx,
            value: u32::MAX,
            len
        })
    );
    assert_eq!(r.tx("CH12").unwrap().wp, 0);
}

#[test]
fn hfc_configs_linux_refuses_are_refused_before_their_write() {
    use crate::tables::{HfcCh, HfcPub};
    let ready = || {
        let mut b = fake(LNK_GEN1);
        let mut st = DmacState::default();
        dmac_pre_init(&mut b, &mut st).unwrap();
        dle_init(&mut b, &mut st, QtaMode::Scc, QtaMode::Scc).unwrap();
        hfc_init(&mut b, &mut st, true, false, false).unwrap();
        (b, st)
    };
    // hfc_pub_cfg_chk (mac.c:961): grp0 + grp1 must be pub_max
    let (mut b, mut st) = ready();
    st.hfc.pub_cfg = Some(HfcPub {
        grp0: 400,
        grp1: 0,
        pub_max: 446,
        wp_thrd: 0,
    });
    let t0 = b.trace.len();
    assert_eq!(
        hfc_init(&mut b, &mut st, false, false, false),
        Err(Rtw89Error::HfcConfig {
            what: "hfc: grp0 + grp1 is not pub_max",
            ch: 12,
            value: 400
        })
    );
    let o = ops(&b, t0);
    assert_eq!(
        o.last().unwrap().line(),
        rd(4, R_AX_DMAC_FUNC_EN).line(),
        "after the DMAC check"
    );
    assert!(
        !o.iter()
            .any(|x| x.line() == w(4, R_AX_PUB_PAGE_CTRL1, 400).line())
    );
    // hfc_ch_cfg_chk (mac.c:926): min below ch011_prec, max above pub_max, group past grp_num
    let bad: [(HfcCh, &str, u32); 3] = [
        (
            HfcCh {
                min: 1,
                max: 10,
                grp: 0,
            },
            "hfc: channel min pages below ch011_prec",
            1,
        ),
        (
            HfcCh {
                min: 5,
                max: 447,
                grp: 0,
            },
            "hfc: channel max pages above pub_max",
            447,
        ),
        (
            HfcCh {
                min: 5,
                max: 10,
                grp: 2,
            },
            "hfc: channel group out of range",
            2,
        ),
    ];
    for (c, what, value) in bad {
        let (mut b, mut st) = ready();
        let mut chs: Vec<HfcCh> = crate::tables::dmac::HFC_CH.to_vec();
        chs[2] = c;
        st.hfc.ch = Some(chs.leak());
        let t0 = b.trace.len();
        assert_eq!(
            hfc_init(&mut b, &mut st, false, false, false),
            Err(Rtw89Error::HfcConfig { what, ch: 2, value })
        );
        // ACH0 and ACH1 written, ACH2 checked and refused
        let o = ops(&b, t0);
        assert_eq!(o.last().unwrap().line(), rd(4, R_AX_DMAC_FUNC_EN).line());
        assert!(!o.iter().any(|x| x.a == R_AX_ACH0_PAGE_CTRL + 8));
    }
    // min 0 is allowed (Linux tests `min &&`)
    let (mut b, mut st) = ready();
    let mut chs: Vec<HfcCh> = crate::tables::dmac::HFC_CH.to_vec();
    chs[2] = HfcCh {
        min: 0,
        max: 10,
        grp: 1,
    };
    st.hfc.ch = Some(chs.leak());
    hfc_init(&mut b, &mut st, false, false, false).unwrap();
}

#[test]
fn dle_configs_linux_refuses_take_its_error_way() {
    use super::dmac::dle_init_rows;
    use crate::tables::DleMem;
    use crate::tables::dmac::DLE_MEM;
    let error_tail = |b: &MemBus, t0: usize| {
        let o = ops(b, t0);
        let n = o.len();
        same(
            &o[n - 3..],
            &[
                clr(4, R_AX_DMAC_FUNC_EN, B_AX_DLE_WDE_EN | B_AX_DLE_PLE_EN),
                rd(4, R_AX_WDE_INI_STATUS),
                rd(4, R_AX_PLE_INI_STATUS),
            ],
            "mac.c error: DLE off, both INI_STATUS read",
        );
        o
    };
    let scc = *DLE_MEM.iter().find(|m| m.mode == "SCC").unwrap();
    let cases: [(DleMem, &str, u32, bool); 3] = {
        let mut size = scc;
        size.ple_size.lnk_pge_num -= 1;
        let mut wde256 = scc;
        // the same bytes in 256-byte pages: passes the size check, refused by dle_mix_cfg
        wde256.wde_size.pge_size = 256;
        wde256.wde_size.lnk_pge_num = 127;
        wde256.wde_size.unlnk_pge_num = 1;
        let mut ple64 = scc;
        ple64.ple_size.pge_size = 64;
        ple64.ple_size.lnk_pge_num = 992;
        ple64.ple_size.unlnk_pge_num = 32;
        [
            (size, "wd/dle mem cfg", 0, false),
            (wde256, "WDE DLE page size 256 not supported", 256, true),
            (ple64, "PLE DLE page size not supported", 64, true),
        ]
    };
    for (row, what, value, mixed) in cases {
        let mut b = fake(LNK_GEN1);
        let mut st = DmacState::default();
        dmac_pre_init(&mut b, &mut st).unwrap();
        b.forced.insert(R_AX_WDE_INI_STATUS, 0x11);
        b.forced.insert(R_AX_PLE_INI_STATUS, 0x22);
        let t0 = b.trace.len();
        let e = dle_init_rows(&mut b, &mut st, QtaMode::Scc, QtaMode::Scc, &[row]).unwrap_err();
        match e {
            Rtw89Error::DleConfig {
                what: w_,
                value: v,
                wde_ini_status: 0x11,
                ple_ini_status: 0x22,
            } => {
                assert_eq!(w_, what);
                if value != 0 {
                    assert_eq!(v, value);
                }
            }
            other => panic!("{what}: {other:?}"),
        }
        let o = error_tail(&b, t0);
        // the size check fails before dle_mix_cfg; the page sizes inside it, after its read
        assert_eq!(
            o.iter().any(|x| x.a == R_AX_WDE_PKTBUF_CFG),
            mixed,
            "{what}"
        );
    }
    // no row for the mode: the same way
    let mut b = fake(LNK_GEN1);
    let mut st = DmacState::default();
    dmac_pre_init(&mut b, &mut st).unwrap();
    let t0 = b.trace.len();
    assert!(matches!(
        dle_init_rows(&mut b, &mut st, QtaMode::Scc, QtaMode::Scc, &[]),
        Err(Rtw89Error::DleConfig { .. })
    ));
    error_tail(&b, t0);
}
