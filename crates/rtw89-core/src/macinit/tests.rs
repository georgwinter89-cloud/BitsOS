//! js/test-macinit.mjs and js/test-macinit-probe.mjs in Rust, check by check: the power
//! switch (normal, efuse invalid, MAC already on, the retry with a failing power-off, a
//! lost bus, the power-off's flags), sys_init and enable_bb_rf, the DMAC/CMAC/TRX init
//! (quota-derived values, the warn-only BA CAM poll), rtw89_mac_init (phases in order,
//! the DMAC check before every HFC write, power off on failure, the page check), vif init
//! with the hooks, the port update (func_sw, the 10 s rule, refused ports), the MACID
//! pause H2C, the core start tail, check_mac_en, sch TX stop/resume (H2C register path,
//! the errors Linux's callers ignore, the register path without firmware) and the RX
//! filter; then the whole stage mac against bringup-golden.txt on the memory bus (with
//! the phy-owned efuse/phycap lines replayed from the golden file). No node needed; the
//! JS modules themselves run the same in tests/macinit_js.rs.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::Cell;
use std::path::PathBuf;

use super::*;
use crate::bringup::{Bringup, RingConfig, power};
use crate::bus::{Bus, BusError, prep};
use crate::fw::hookbus::HookBus;
use crate::fw::{Firmware, FwState};
use crate::membus::MemBus;
use crate::pci::{self, PciRings};
use crate::tables::regs::*;
use crate::trace::{self, Op, OpKind, TraceEntry};

fn wlan_dir() -> PathBuf {
    std::env::var_os("WLAN_TOOLS")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/home/georg/BitsOS/werkzeuge/plattform/wlan".into())
}

pub(super) fn fw_file() -> Firmware {
    let p = wlan_dir().join("firmware/rtw8852b_fw-2.bin");
    let f = std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    Firmware::parse(f, 1, false).expect("the real file")
}

fn golden() -> Vec<String> {
    let p = wlan_dir().join("bringup-golden.txt");
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    text.lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

/// (kind, size, a, b, c, ms) of every op since `from`
type O = (u8, u8, u32, u32, u32, u16);

fn ops_of(t: &[TraceEntry]) -> Vec<O> {
    t.iter()
        .filter_map(|e| match e {
            TraceEntry::Op(o) => Some((o.kind as u8, o.size, o.a, o.b, o.c, o.ms)),
            TraceEntry::Note(_) => None,
        })
        .collect()
}

fn ops(b: &HookBus<'_>, from: usize) -> Vec<O> {
    ops_of(&b.mem.trace[from..])
}

/// the ops `f` queues on a fresh memory bus (test-macinit `opsOf`)
fn ops_on_fresh(f: impl FnOnce(&mut MemBus)) -> Vec<O> {
    let mut m = MemBus::new();
    f(&mut m);
    ops_of(&m.trace)
}

const RD: u8 = OpKind::Rd as u8;
const WR: u8 = OpKind::Wr as u8;
const RMW: u8 = OpKind::Rmw as u8;
const POLL: u8 = OpKind::Poll as u8;
const DELAY: u8 = OpKind::Delay as u8;

fn rd32(a: u32) -> O {
    (RD, 4, a, 0, 0, 0)
}
fn wr(size: u8, a: u32, v: u32) -> O {
    (WR, size, a, 0, v, 0)
}
fn rmw(size: u8, a: u32, clr: u32, set: u32) -> O {
    (RMW, size, a, clr, set, 0)
}
fn set32(a: u32, bits: u32) -> O {
    rmw(4, a, 0, bits)
}
fn clr32(a: u32, bits: u32) -> O {
    rmw(4, a, bits, 0)
}
fn mask32(a: u32, mask: u32, v: u32) -> O {
    rmw(4, a, mask, prep(mask, v))
}

const CMAC_EN: O = (RD, 4, R_AX_CMAC_FUNC_EN, 0, 0, 0);
const DMAC_EN: O = (RD, 4, R_AX_DMAC_FUNC_EN, 0, 0, 0);

/// test-macinit `fake`: config space with the PCIe capability at 0x70 and the link at gen 1,
/// the BA CAM reset done, all public pages free (hfc_pub_info_chk, mac.c:945)
fn fake<'a>() -> HookBus<'a> {
    let mut m = MemBus::new();
    for (off, v) in [
        (0x04u16, 0x0010_0002u32),
        (0x34, 0x70),
        (0x70, 0x0002_0010),
        (0x80, 0x1011_0000),
    ] {
        m.cfg.insert(off, v);
    }
    m.forced.insert(R_AX_RESPBA_CAM_CTRL, 0);
    m.forced.insert(R_AX_PUB_PAGE_INFO2, 446);
    HookBus::new(m)
}

/// test-macinit `enabled`: DMAC and CMAC read as enabled, the trace empty
fn enabled(b: &mut HookBus<'_>) {
    b.w32(R_AX_DMAC_FUNC_EN, B_AX_MAC_FUNC_EN | B_AX_DMAC_FUNC_EN)
        .unwrap();
    b.w32(R_AX_CMAC_FUNC_EN, B_AX_CMAC_EN).unwrap();
    b.mem.trace.clear();
}

/// the efuse of the measured chip: rfe 1, autoloaded, no power K
pub(super) const OPTS: PwrOpts = PwrOpts {
    rfe: 1,
    efuse_valid: true,
    power_k_valid: false,
    cv: 1,
    probe: false,
};

fn pwr_on_ops(o: &PwrOpts) -> Vec<O> {
    ops_on_fresh(|m| power::pwr_on_func(m, o).unwrap())
}
fn pwr_off_ops(rfe: u8) -> Vec<O> {
    ops_on_fresh(|m| power::pwr_off_func(m, rfe).unwrap())
}
fn scoreboard(v: u32) -> O {
    wr(1, R_AX_SCOREBOARD + 3, v)
}

// ---- power switch (mac.c:1521, 1588): normal, efuse invalid, "MAC already on", errors ----

#[test]
fn power_on_reads_the_state_runs_pwr_on_func_and_sets_the_scoreboard() {
    let mut b = fake();
    let (mut m, mut fw) = (MacState::new(), FwState::new());
    mac_preinit(&mut b, &OPTS, &mut m, &mut fw).unwrap();
    let o = ops(&b, 0);
    let on = pwr_on_ops(&OPTS);
    assert_eq!(o[0], rd32(R_AX_IC_PWR_STATE));
    assert_eq!(&o[1..1 + on.len()], &on[..]);
    assert_eq!(o[1 + on.len()..], [scoreboard(MAC_AX_NOTIFY_TP_MAJOR)]);
    assert!(
        on.iter().any(|x| x.2 == R_AX_HCI_LDO_CTRL),
        "cut B voltage trim without power K"
    );
    assert_eq!(m.power_on, Some(true));
    assert!(m.events().is_empty());
}

#[test]
fn an_efuse_not_autoloaded_has_no_voltage_trims() {
    let mut b = fake();
    let o = PwrOpts {
        efuse_valid: false,
        ..OPTS
    };
    mac_preinit(&mut b, &o, &mut MacState::new(), &mut FwState::new()).unwrap();
    let got = ops(&b, 0);
    assert_eq!(got[1..got.len() - 1], pwr_on_ops(&o)[..]);
    assert!(!got.iter().any(|x| x.2 == R_AX_HCI_LDO_CTRL));
    assert!(
        !got.iter()
            .any(|x| x.2 == R_AX_SPS_DIG_ON_CTRL0 && x.3 == B_AX_VOL_L1_MASK),
        "no VOL_L1 trim"
    );
}

#[test]
fn mac_already_on_powers_off_and_on_again() {
    let reads = Cell::new(0);
    let mut b = fake();
    b.on_read(|a| {
        (a == R_AX_IC_PWR_STATE && {
            reads.set(reads.get() + 1);
            reads.get() == 1
        })
        .then_some(0x100)
    });
    let o = PwrOpts {
        power_k_valid: true,
        ..OPTS
    };
    let (mut m, mut fw) = (MacState::new(), FwState::new());
    fw.ready = true;
    mac_preinit(&mut b, &o, &mut m, &mut fw).unwrap();
    let off = pwr_off_ops(1);
    let on = pwr_on_ops(&o);
    let mut want = vec![rd32(R_AX_IC_PWR_STATE)];
    want.extend(off);
    want.push(scoreboard(MAC_AX_NOTIFY_PWR_MAJOR));
    want.push(rd32(R_AX_IC_PWR_STATE));
    want.extend(on.iter().copied());
    want.push(scoreboard(MAC_AX_NOTIFY_TP_MAJOR));
    assert_eq!(ops(&b, 0), want);
    assert!(
        !on.iter().any(|x| x.2 == R_AX_HCI_LDO_CTRL),
        "no trim with power K"
    );
    assert!(matches!(
        m.events(),
        [MacEvent::PwrOnRetry {
            error: Rtw89Error::MacAlreadyOn { .. }
        }]
    ));
    assert!(!fw.ready, "the power-off in between clears FW_RDY");
    assert_eq!(m.power_on, Some(true));
}

#[test]
fn a_stopped_power_on_is_retried_after_a_power_off_whose_failure_is_ignored() {
    let fails = core::cell::RefCell::new(vec![B_AX_APFN_ONMAC, B_AX_APFM_OFFMAC]);
    let mut b = fake();
    b.on_poll(|a, mask| {
        let mut f = fails.borrow_mut();
        Ok(
            match f.iter().position(|&x| a == R_AX_SYS_PW_CTRL && x == mask) {
                Some(i) => {
                    f.remove(i);
                    false
                }
                None => true,
            },
        )
    });
    let mut m = MacState::new();
    mac_preinit(&mut b, &OPTS, &mut m, &mut FwState::new()).unwrap();
    assert!(fails.borrow().is_empty());
    let lines = m.event_lines();
    assert!(lines[0].contains("power on failed"), "{lines:?}");
    assert!(lines[1].contains("power off failed too"), "{lines:?}");
    let o = ops(&b, 0);
    let second = o
        .iter()
        .rposition(|x| *x == rd32(R_AX_IC_PWR_STATE))
        .unwrap();
    assert_eq!(
        o[second + 1..o.len() - 1],
        pwr_on_ops(&OPTS)[..],
        "the second power on is complete"
    );
    assert_eq!(m.power_on, Some(true));
}

#[test]
fn a_lost_bus_is_no_reason_for_an_off_on_cycle() {
    let mut b = fake();
    b.on_poll(|a, _| {
        if a == R_AX_SYS_PW_CTRL {
            Err(BusError::Gone)
        } else {
            Ok(true)
        }
    });
    let mut m = MacState::new();
    let e = mac_preinit(&mut b, &OPTS, &mut m, &mut FwState::new()).unwrap_err();
    assert_eq!(e, Rtw89Error::Bus(BusError::Gone));
    assert!(m.events().is_empty());
    assert_eq!(
        ops(&b, 0)
            .iter()
            .filter(|x| x.2 == R_AX_IC_PWR_STATE)
            .count(),
        1,
        "no second power-on"
    );
    assert_eq!(m.power_on, None, "unknown");
}

#[test]
fn power_off_clears_fw_ready_and_the_power_flag() {
    let mut b = fake();
    let (mut m, mut fw) = (MacState::new(), FwState::new());
    fw.ready = true;
    m.power_on = Some(true);
    mac_pwr_off(&mut b, 1, &mut m, &mut fw).unwrap();
    assert_eq!((fw.ready, m.power_on), (false, Some(false)));
    let mut want = pwr_off_ops(1);
    want.push(scoreboard(MAC_AX_NOTIFY_PWR_MAJOR));
    assert_eq!(ops(&b, 0), want);
}

// ---- the probe power-on (test-macinit-probe.mjs): the secure-boot bytes, read again on a retry ----

#[test]
fn the_probe_retry_reads_the_secure_boot_bytes_again() {
    let once = Cell::new(true);
    let mut b = fake();
    b.mem.forced.insert(R_AX_EFUSE_CTRL, B_AX_EF_RDY | 0xff);
    b.on_poll(|a, mask| {
        Ok(!(a == R_AX_SYS_PW_CTRL && mask == B_AX_APFN_ONMAC && once.replace(false)))
    });
    let (mut m, mut fw) = (MacState::new(), FwState::new());
    let p = mac_pwr_on(&mut b, &PwrOpts::probe(1), &mut m, &mut fw).unwrap();
    assert_eq!(p.secure_boot, Some(false), "FF FF: no secure boot");
    let on = pwr_on_ops(&PwrOpts::probe(1));
    let sec = ops_on_fresh(|x| {
        x.forced.insert(R_AX_EFUSE_CTRL, B_AX_EF_RDY | 0xff);
        crate::bringup::efuse::read_fw_secure(x, 1).unwrap();
    });
    let o = ops(&b, 0);
    let again = o
        .iter()
        .rposition(|x| *x == rd32(R_AX_IC_PWR_STATE))
        .unwrap();
    let off = pwr_off_ops(0);
    assert_eq!(
        o[again - off.len() - 1..again - 1],
        off[..],
        "power off before the retry"
    );
    let mut want = on.clone();
    want.extend(sec);
    assert_eq!(
        o[again + 1..o.len() - 1],
        want[..],
        "the retry reads 0x5EC/0x5ED again"
    );
    assert_eq!(o.last(), Some(&scoreboard(MAC_AX_NOTIFY_TP_MAJOR)));
}

// ---- sys_init_ax and __rtw8852bx_mac_enable_bb_rf, values included ----

#[test]
fn sys_init_and_enable_bb_rf_write_the_c_values() {
    let mut b = fake();
    sys_init(&mut b).unwrap();
    assert_eq!(
        ops(&b, 0),
        [
            wr(
                4,
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
                    | B_AX_DMAC_CRPRT
            ),
            wr(
                4,
                R_AX_DMAC_CLK_EN,
                B_AX_MAC_SEC_CLK_EN
                    | B_AX_DISPATCHER_CLK_EN
                    | B_AX_DLE_CPUIO_CLK_EN
                    | B_AX_PKT_IN_CLK_EN
                    | B_AX_STA_SCH_CLK_EN
                    | B_AX_TXPKT_CTRL_CLK_EN
                    | B_AX_WD_RLS_CLK_EN
                    | B_AX_BBRPT_CLK_EN
            ),
            set32(
                R_AX_CK_EN,
                B_AX_CMAC_CKEN
                    | B_AX_PHYINTF_CKEN
                    | B_AX_CMAC_DMA_CKEN
                    | B_AX_PTCLTOP_CKEN
                    | B_AX_SCHEDULER_CKEN
                    | B_AX_TMAC_CKEN
                    | B_AX_RMAC_CKEN
            ),
            set32(
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
                    | B_AX_CMAC_CRPRT
            ),
            set32(R_AX_SPS_DIG_ON_CTRL0, B_AX_OCP_L1_MASK),
        ]
    );
    let mut e = fake();
    enable_bb_rf(&mut e).unwrap();
    let xtal = |off: u32| {
        [
            wr(
                4,
                R_AX_WLAN_XTAL_SI_CTRL,
                prep(B_AX_WL_XTAL_SI_ADDR_MASK, off)
                    | prep(B_AX_WL_XTAL_SI_DATA_MASK, 0xC7)
                    | prep(B_AX_WL_XTAL_SI_BITMASK_MASK, FULL_BIT_MASK)
                    | prep(B_AX_WL_XTAL_SI_MODE_MASK, XTAL_SI_NORMAL_WRITE)
                    | B_AX_WL_XTAL_SI_CMD_POLL,
            ),
            (
                POLL,
                4,
                R_AX_WLAN_XTAL_SI_CTRL,
                B_AX_WL_XTAL_SI_CMD_POLL,
                0,
                50,
            ),
        ]
    };
    let mut want = vec![
        rmw(
            1,
            R_AX_SYS_FUNC_EN,
            0,
            B_AX_FEN_BBRSTB | B_AX_FEN_BB_GLB_RSTN,
        ),
        mask32(R_AX_SPS_DIG_ON_CTRL0, B_AX_REG_ZCDC_H_MASK, 1),
        set32(R_AX_WLRF_CTRL, B_AX_AFC_AFEDIG),
        clr32(R_AX_WLRF_CTRL, B_AX_AFC_AFEDIG),
        set32(R_AX_WLRF_CTRL, B_AX_AFC_AFEDIG),
    ];
    want.extend(xtal(XTAL_SI_WL_RFC_S0));
    want.extend(xtal(XTAL_SI_WL_RFC_S1));
    want.push(wr(1, R_AX_PHYREG_SET, PHYREG_SET_XYN_CYCLE));
    assert_eq!(ops(&e, 0), want);
}

// ---- dmac_init_ax / cmac_init_ax / trx_init_ax ----

#[test]
fn dmac_init_tail_and_the_dmac_check_before_each_hfc_write() {
    let mut b = fake();
    enabled(&mut b);
    let mut m = MacState::new();
    dmac_init(&mut b, &mut m).unwrap();
    let o = ops(&b, 0);
    let i = o.iter().position(|x| x.2 == R_AX_SS_CTRL).unwrap() - 1;
    let ss = |bits| set32(R_AX_SS_CTRL, bits);
    assert_eq!(
        o[i..],
        [
            DMAC_EN,
            rmw(1, R_AX_SS_CTRL, 0, B_AX_SS_EN),
            (
                POLL,
                4,
                R_AX_SS_CTRL,
                B_AX_SS_INIT_DONE_1,
                B_AX_SS_INIT_DONE_1,
                2
            ),
            ss(B_AX_SS_WARM_INIT_FLG),
            clr32(R_AX_SS_CTRL, B_AX_SS_NONEMPTY_SS2FINFO_EN),
            DMAC_EN,
            wr(4, R_AX_ACTION_FWD0, TRXCFG_MPDU_PROC_ACT_FRWD),
            wr(4, R_AX_TF_FWD, TRXCFG_MPDU_PROC_TF_FRWD),
            set32(R_AX_MPDU_PROC, B_AX_APPEND_FCS | B_AX_A_ICV_ERR),
            wr(4, R_AX_CUT_AMSDU_CTRL, TRXCFG_MPDU_PROC_CUT_CTRL),
            DMAC_EN,
            rmw(
                4,
                R_AX_SEC_ENG_CTRL,
                B_AX_TX_PARTIAL_MODE
                    | B_AX_CLK_EN_CGCMP
                    | B_AX_CLK_EN_WAPI
                    | B_AX_CLK_EN_WEP_TKIP
                    | B_AX_SEC_TX_ENC
                    | B_AX_SEC_RX_DEC
                    | B_AX_MC_DEC
                    | B_AX_BC_DEC,
                B_AX_CLK_EN_CGCMP
                    | B_AX_CLK_EN_WAPI
                    | B_AX_CLK_EN_WEP_TKIP
                    | B_AX_SEC_TX_ENC
                    | B_AX_SEC_RX_DEC
                    | B_AX_MC_DEC
                    | B_AX_BC_DEC
            ),
            set32(R_AX_SEC_MPDU_PROC, B_AX_APPEND_ICV | B_AX_APPEND_MIC),
        ]
    );
    // hfc_ch_ctrl / hfc_pub_ctrl: each write right after its DMAC check
    let hfc: Vec<usize> = o
        .iter()
        .enumerate()
        .filter(|(_, x)| {
            x.0 == WR
                && ((R_AX_ACH0_PAGE_CTRL..R_AX_ACH0_PAGE_CTRL + 48).contains(&x.2)
                    || x.2 == R_AX_PUB_PAGE_CTRL1)
        })
        .map(|(i, _)| i)
        .collect();
    assert_eq!(hfc.len(), 7, "ACH0-3, CH8, CH9 and the public pages");
    assert!(hfc.iter().all(|&i| o[i - 1] == DMAC_EN));
    let info = m.dmac.dle_info.unwrap();
    assert_eq!((info.ple_pg_size, info.c0_rx_qta), (128, 89));
}

#[test]
fn cmac_init_takes_the_filter_and_the_rx_quota() {
    assert_eq!(trx::rx_max_len(89, 128), 22);
    assert_eq!(
        trx::rx_max_len(1000, 64),
        15,
        "at most PLD_RLS_MAX_PG pages"
    );
    assert_eq!(trx::rx_max_len(1000, 128), 23, "at most RX_SPEC_MAX_LEN");
    assert_eq!(trx::rx_max_len(u32::MAX, u32::MAX), 23, "no overflow");
    let mut b = fake();
    enabled(&mut b);
    let mut m = MacState::new();
    m.dmac.dle_info = Some(pci::dmac::DleInfo {
        qta_mode: pci::QtaMode::Scc,
        ple_pg_size: 128,
        ple_free_pg: 0,
        c0_rx_qta: 89,
        c1_rx_qta: 0,
    });
    cmac_init(&mut b, &mut m).unwrap();
    let o = ops(&b, 0);
    assert!(
        o.contains(&wr(4, R_AX_RX_FLTR_OPT, 0x0300_44BE)),
        "DEFAULT_AX_RX_FLTR"
    );
    assert!(o.contains(&mask32(R_AX_RX_FLTR_OPT, B_AX_RX_MPDU_MAX_LEN_MASK, 22)));
    assert_eq!(o.last(), Some(&rmw(1, R_AX_RXDMA_CTRL_0, RX_FULL_MODE, 0)));
    assert_eq!(o.iter().filter(|x| **x == CMAC_EN).count(), 11);
    // without the DLE init there is no RX quota: an error, never a guess
    let mut n = fake();
    enabled(&mut n);
    let e = cmac_init(&mut n, &mut MacState::new()).unwrap_err();
    assert!(matches!(e, Rtw89Error::Invalid { .. }));
}

#[test]
fn a_ba_cam_reset_that_never_ends_only_warns_and_is_bounded() {
    let busy = prep(B_AX_BACAM_RST_MASK, S_AX_BACAM_RST_ALL);
    let mut b = fake();
    b.mem.forced.insert(R_AX_RESPBA_CAM_CTRL, busy);
    enabled(&mut b);
    let mut m = MacState::new();
    m.dmac.dle_info = Some(pci::dmac::DleInfo {
        qta_mode: pci::QtaMode::Scc,
        ple_pg_size: 128,
        ple_free_pg: 0,
        c0_rx_qta: 89,
        c1_rx_qta: 0,
    });
    cmac_init(&mut b, &mut m).unwrap();
    assert_eq!(
        m.events(),
        [MacEvent::BacamResetTimeout {
            reg: R_AX_RESPBA_CAM_CTRL,
            value: busy
        }]
    );
    let o = ops(&b, 0);
    let reads = o
        .iter()
        .filter(|x| **x == rd32(R_AX_RESPBA_CAM_CTRL))
        .count();
    let delays = o.iter().filter(|x| x.0 == DELAY && x.2 == 1).count();
    // the memory bus's clock moves only by whole ms: 1000 waited steps of 1 us, then one more read
    assert_eq!((reads, delays), (1002, 1000));
    assert_eq!(
        o.last(),
        Some(&rmw(1, R_AX_RXDMA_CTRL_0, RX_FULL_MODE, 0)),
        "cmac_init went on"
    );
    // and it reads again until the time is up: first busy, then done
    let n = Cell::new(0);
    let mut p = fake();
    p.on_read(|a| {
        (a == R_AX_RESPBA_CAM_CTRL && {
            n.set(n.get() + 1);
            n.get() == 1
        })
        .then_some(busy)
    });
    assert_eq!(
        io::poll_warn(&mut p, R_AX_RESPBA_CAM_CTRL, B_AX_BACAM_RST_MASK, 0, 1000).unwrap(),
        None
    );
    assert_eq!(
        ops(&p, 0),
        [
            rd32(R_AX_RESPBA_CAM_CTRL),
            (DELAY, 0, 1, 0, 0, 0),
            rd32(R_AX_RESPBA_CAM_CTRL)
        ]
    );
}

#[test]
fn trx_init_ends_with_the_error_masks_and_the_release_reports() {
    let mut b = fake();
    enabled(&mut b);
    trx_init(&mut b, &mut MacState::new()).unwrap();
    let o = ops(&b, 0);
    let tail = &o[o.len() - 6..];
    assert_eq!(
        tail,
        [
            wr(4, R_AX_DMAC_ERR_IMR, DMAC_ERR_IMR_EN),
            wr(4, R_AX_CMAC_ERR_IMR, CMAC0_ERR_IMR_EN),
            mask32(R_AX_WDRLS_CFG, B_AX_WDRLS_MODE_MASK, RTW89_RPR_MODE_POH),
            set32(R_AX_RLSRPT0_CFG0, B_AX_RLSRPT0_FLTR_MAP_MASK),
            mask32(R_AX_RLSRPT0_CFG1, B_AX_RLSRPT0_AGGNUM_MASK, 30),
            mask32(R_AX_RLSRPT0_CFG1, B_AX_RLSRPT0_TO_MASK, 255),
        ]
    );
    let i = o.iter().position(|x| x.2 == R_AX_WDRLS_ERR_IMR).unwrap();
    assert_eq!(o[i - 1], DMAC_EN);
    assert_eq!(
        o[i..i + 2],
        [
            clr32(R_AX_WDRLS_ERR_IMR, B_AX_WDRLS_IMR_EN_CLR),
            set32(R_AX_WDRLS_ERR_IMR, B_AX_WDRLS_IMR_SET)
        ]
    );
    let c = o.iter().position(|x| x.2 == R_AX_SCHEDULE_ERR_IMR).unwrap();
    assert_eq!(o[c - 1], CMAC_EN);
    assert!(
        o.contains(&clr32(R_AX_PHYINFO_ERR_IMR, 0)) && o.contains(&set32(R_AX_PHYINFO_ERR_IMR, 0))
    );
}

// ---- rtw89_mac_init (mac.c:4360): phases in order, power off on failure, the page check ----

struct Run<'a> {
    b: HookBus<'a>,
    rings: PciRings,
    m: MacState,
    fws: FwState,
}

fn run<'a>() -> Run<'a> {
    let mut b = fake();
    let rings = pci::pci_alloc(&mut b, &RingConfig::js()).unwrap();
    b.mem.trace.clear();
    Run {
        b,
        rings,
        m: MacState::new(),
        fws: FwState::new(),
    }
}

impl Run<'_> {
    fn mac_init(&mut self, fw: &Firmware, preinit: bool) -> Rtw89Result<()> {
        let c = MacInit {
            rings: &mut self.rings,
            mac: &mut self.m,
            fws: &mut self.fws,
            fw,
            opts: &OPTS,
        };
        super::mac_init(&mut self.b, c, preinit)
    }
}

#[test]
fn mac_init_runs_its_phases_in_order() {
    let fw = fw_file();
    let mut r = run();
    r.mac_init(&fw, true).unwrap();
    let o = ops(&r.b, 0);
    let find = |f: &dyn Fn(&O) -> bool| o.iter().position(f).unwrap();
    let steps = [
        find(&|x| *x == rd32(R_AX_IC_PWR_STATE)),
        find(&|x| x.2 == R_AX_HCI_FUNC_EN),
        find(&|x| x.2 == R_AX_DMAC_FUNC_EN && x.0 == WR),
        find(&|x| x.2 == R_AX_PCIE_PS_CTRL),
        find(&|x| x.2 == R_AX_WCPU_FW_CTRL && x.0 == POLL),
        find(&|x| {
            x.2 == R_AX_SYS_FUNC_EN && x.0 == RMW && x.4 == B_AX_FEN_BBRSTB | B_AX_FEN_BB_GLB_RSTN
        }),
        find(&|x| x.2 == R_AX_SS_CTRL),
        find(&|x| x.2 == R_AX_PREBKF_CFG_1),
        find(&|x| *x == wr(4, R_AX_DMAC_ERR_IMR, DMAC_ERR_IMR_EN)),
        o.iter()
            .rposition(|x| x.2 == R_AX_TX_ADDRESS_INFO_MODE_SETTING)
            .unwrap(),
        o.len() - 1,
    ];
    assert!(
        steps.windows(2).all(|w| w[0] < w[1]),
        "macInit phases out of order: {steps:?}"
    );
    // enable_bb_rf and sys_init right after the download (its status poll)
    let bb = steps[5];
    assert_eq!(o[bb - 1].2, R_AX_WCPU_FW_CTRL, "after rtw89_fw_check_rdy");
    assert_eq!(
        o[bb + 10].2,
        R_AX_DMAC_FUNC_EN,
        "sys_init after the 10 ops of enable_bb_rf"
    );
    // the offload config H2C ends it, with the CH12 kick and fetch
    assert_eq!(o[o.len() - 2].2, R_AX_CH12_TXBD_IDX);
    assert!(r.fws.ready && r.fws.h2c_seq == 1, "{:?}", r.fws);
    assert_eq!(r.m.power_on, Some(true));
    // download quotas first (dmac_pre_init: DLFW), SCC in dmac_init
    assert_eq!(r.m.dmac.dle_info.unwrap().qta_mode, pci::QtaMode::Scc);
}

#[test]
fn a_failing_mac_init_powers_off_itself() {
    let fw = fw_file();
    let mut r = run();
    r.b.on_poll(|a, _| Ok(a != R_AX_SS_CTRL));
    let e = r.mac_init(&fw, true).unwrap_err();
    assert!(
        matches!(e, Rtw89Error::Bus(BusError::PollTimeout { addr, .. }) if addr == R_AX_SS_CTRL)
    );
    let o = ops(&r.b, 0);
    let off = pwr_off_ops(1);
    assert_eq!(
        o[o.len() - off.len() - 1..o.len() - 1],
        off[..],
        "mac init failure powers off"
    );
    assert_eq!(o.last(), Some(&scoreboard(MAC_AX_NOTIFY_PWR_MAJOR)));
    assert!(!r.fws.ready, "the power off clears the firmware-ready flag");
    assert_eq!(r.m.power_on, Some(false));
    assert!(matches!(r.m.events(), [MacEvent::MacInitFailed { .. }]));
}

#[test]
fn public_pages_that_do_not_add_up_fail_mac_init_which_powers_off() {
    let fw = fw_file();
    let mut r = run();
    r.b.mem.forced.insert(R_AX_PUB_PAGE_INFO2, 445);
    enabled(&mut r.b);
    let e = r.mac_init(&fw, false).unwrap_err();
    assert!(
        matches!(e, Rtw89Error::HfcPubPages { pub_aval: 445, .. }),
        "{e:?}"
    );
    assert_eq!(r.m.power_on, Some(false), "mac_init failed: powered off");
    assert!(r.m.event_lines()[0].contains("mac init failed"));
    let o = ops(&r.b, 0);
    let off = pwr_off_ops(1);
    assert_eq!(
        o[o.len() - off.len() - 1..o.len() - 1],
        off[..],
        "pwr_off_func at the end"
    );
}

// ---- rtw89_mac_vif_init (mac.c:5045) with sta's hooks, rtw89_mac_port_update (mac.c:5104) ----

/// sta's hooks, each a recognisable register write (test-macinit's marks) and a record
#[derive(Default)]
struct Hooks {
    calls: Vec<String>,
}

const HOOK_MARK: u32 = 0x700;

impl Hooks {
    fn mark<B: Bus + ?Sized>(&mut self, bus: &mut B, what: String) -> Rtw89Result<()> {
        bus.w32(HOOK_MARK + 4 * self.calls.len() as u32, 1)?;
        self.calls.push(what);
        Ok(())
    }
}

impl<B: Bus + ?Sized> VifHooks<B> for Hooks {
    fn macid_pause(&mut self, bus: &mut B, sh: u32, grp: u32, pause: bool) -> Rtw89Result<()> {
        self.mark(bus, alloc::format!("macidPause {sh} {grp} {pause}"))
    }
    fn role_maintain(&mut self, bus: &mut B, upd: u32) -> Rtw89Result<()> {
        self.mark(bus, alloc::format!("roleMaintain {upd}"))
    }
    fn join_info(&mut self, bus: &mut B, dis_conn: bool) -> Rtw89Result<()> {
        self.mark(bus, alloc::format!("joinInfo {dis_conn}"))
    }
    fn cam_init(&mut self, bus: &mut B) -> Rtw89Result<()> {
        self.mark(bus, "camInit".into())
    }
    fn addr_cam(&mut self, bus: &mut B, upd: u32) -> Rtw89Result<()> {
        self.mark(bus, alloc::format!("addrCam {upd}"))
    }
    fn default_cmac_tbl(&mut self, bus: &mut B) -> Rtw89Result<()> {
        self.mark(bus, "defaultCmacTbl".into())
    }
}

/// macinit-spec.mjs `portUpdateSpecs` for a station, NO_LINK or INFRA, port 0-3
fn port_update_want(v: &Vif, func_sw: bool) -> Vec<O> {
    let p = |reg: u32| reg + u32::from(v.port) * 0x40;
    let cfg = p(R_AX_PORT_CFG_P0);
    let infra = v.net_type == RTW89_NET_TYPE_INFRA;
    let en = |on: bool, bits: u32| {
        if on {
            set32(cfg, bits)
        } else {
            clr32(cfg, bits)
        }
    };
    let mut w = vec![rd32(cfg)];
    if func_sw {
        w.extend([
            (DELAY, 0, (u32::from(v.beacon_int) + 1) * 1000, 0, 0, 0),
            clr32(cfg, B_AX_PORT_FUNC_EN | B_AX_BRK_SETUP),
            set32(cfg, B_AX_TSFTR_RST),
            wr(4, p(R_AX_BCN_CNT_TMR_P0), 0),
        ]);
    }
    let hiq = [
        R_AX_P0MB_HGQ_WINDOW_CFG_0,
        R_AX_PORT_HGQ_WINDOW_CFG,
        R_AX_PORT_HGQ_WINDOW_CFG + 1,
        R_AX_PORT_HGQ_WINDOW_CFG + 2,
    ][usize::from(v.port)];
    let color = [
        B_AX_BSS_COLOB_AX_PORT_0_MASK,
        B_AX_BSS_COLOB_AX_PORT_1_MASK,
        B_AX_BSS_COLOB_AX_PORT_2_MASK,
        B_AX_BSS_COLOB_AX_PORT_3_MASK,
    ][usize::from(v.port)];
    let intv = if v.beacon_int != 0 {
        v.beacon_int.into()
    } else {
        100
    };
    w.extend([
        clr32(cfg, B_AX_TXBCN_RPT_EN),
        clr32(cfg, B_AX_RXBCN_RPT_EN),
        mask32(cfg, B_AX_NET_TYPE_MASK, v.net_type),
        en(infra, B_AX_TBTT_PROHIB_EN | B_AX_BRK_SETUP),
        en(infra, B_AX_RX_BSSID_FIT_EN),
        en(infra, B_AX_TSF_UDT_EN),
        en(false, B_AX_BCNTX_EN),
        mask32(p(R_AX_BCN_SPACE_CFG_P0), B_AX_BCN_SPACE_MASK, intv),
        wr(1, hiq, 0),
        rmw(1, R_AX_MD_TSFT_STMP_CTL, 0, B_AX_UPD_HGQMD | B_AX_UPD_TIMIE),
        rmw(
            2,
            p(R_AX_DTIM_CTRL_P0),
            B_AX_DTIM_NUM_MASK,
            prep(B_AX_DTIM_NUM_MASK, v.dtim_period.into()),
        ),
        clr32(
            R_AX_MBSSID_DROP_0,
            prep(B_AX_PORT_DROP_4_0_MASK, 1 << v.port) | u32::from(v.port == 0),
        ),
        mask32(p(R_AX_TBTT_PROHIB_P0), B_AX_TBTT_SETUP_MASK, 2),
        mask32(p(R_AX_TBTT_PROHIB_P0), B_AX_TBTT_HOLD_MASK, 200),
        mask32(p(R_AX_BCN_AREA_P0), B_AX_BCN_MSK_AREA_MASK, 0),
        rmw(
            2,
            p(R_AX_TBTTERLYINT_CFG_P0),
            B_AX_TBTTERLY_MASK,
            prep(B_AX_TBTTERLY_MASK, 5),
        ),
        rmw(
            2,
            p(R_AX_TBTT_AGG_P0),
            B_AX_TBTT_AGG_NUM_MASK,
            prep(B_AX_TBTT_AGG_NUM_MASK, 1),
        ),
        mask32(R_AX_PTCL_BSS_COLOR_0, color, v.bss_color.into()),
    ]);
    if v.port == 0 {
        w.push(clr32(R_AX_MBSSID_CTRL, B_AX_P0MB_ALL_MASK));
    }
    w.extend([
        set32(cfg, B_AX_PORT_FUNC_EN),
        (DELAY, 0, 20, 0, 0, 0),
        mask32(p(R_AX_BCNERLYINT_CFG_P0), B_AX_BCNERLY_MASK, 160),
        mask32(
            R_AX_BCN_PSR_RPT_P0 + u32::from(v.port) * 4,
            B_AX_BCAID_P0_MASK,
            0,
        ),
    ]);
    w
}

/// macinit-spec.mjs `VIF_TABLES`: rtw89_mac_dmac_tbl_init's loop and rtw89_mac_cmac_tbl_init
fn vif_tables(mac_id: u32) -> Vec<O> {
    let mut w = Vec::new();
    for i in 0..4 {
        w.push(wr(
            4,
            R_AX_FILTER_MODEL_ADDR,
            DMAC_TBL_BASE_ADDR + (mac_id << 4) + (i << 2),
        ));
        w.push(wr(4, R_AX_INDIR_ACCESS_ENTRY, 0));
    }
    w.push(wr(
        4,
        R_AX_FILTER_MODEL_ADDR,
        CMAC_TBL_BASE_ADDR + mac_id * CCTL_INFO_SIZE,
    ));
    for (i, v) in [0x4, 0x400A_0004, 0, 0, 0, 0xE43_000B, 0, 0xB_8109]
        .into_iter()
        .enumerate()
    {
        w.push(wr(4, R_AX_INDIR_ACCESS_ENTRY + 4 * i as u32, v));
    }
    w
}

#[test]
fn vif_init_updates_the_port_inits_the_tables_and_calls_the_hooks_in_order() {
    for (port, mac_id) in [(0u8, 0u8), (1, 3)] {
        let mut b = fake();
        enabled(&mut b);
        let vif = Vif::station(mac_id, port, [0x02, 0xb1, 0x75, 0x00, 0x00, 0x01]);
        let mut h = Hooks::default();
        vif_init(&mut b, &mut MacState::new(), &vif, &mut h).unwrap();
        let mut want = port_update_want(&vif, false);
        want.extend(vif_tables(mac_id.into()));
        want.push(CMAC_EN);
        want.extend((0..6).map(|n| wr(4, HOOK_MARK + 4 * n, 1)));
        assert_eq!(ops(&b, 0), want, "port {port}");
        assert_eq!(
            h.calls,
            [
                alloc::format!("macidPause {} {} false", mac_id & 31, mac_id >> 5),
                "roleMaintain 0".into(),
                "joinInfo true".into(),
                "camInit".into(),
                "addrCam 0".into(),
                "defaultCmacTbl".into(),
            ]
        );
    }
}

#[test]
fn an_associated_running_port_waits_a_beacon_first_and_long_waits_are_cut() {
    let vif = Vif {
        net_type: RTW89_NET_TYPE_INFRA,
        beacon_int: 102,
        dtim_period: 3,
        bss_color: 5,
        ..Vif::station(3, 1, [0; 6])
    };
    let cfg1 = R_AX_PORT_CFG_P0 + 0x40;
    let mut a = fake();
    a.on_read(|r| (r == cfg1).then_some(B_AX_PORT_FUNC_EN));
    let mut m = MacState::new();
    port_update(&mut a, &mut m, &vif).unwrap();
    let mut want = port_update_want(&vif, true);
    // port 1: bss color mask of port 1 with the value, DTIM 3
    assert_eq!(ops(&a, 0), core::mem::take(&mut want));
    assert!(m.events().is_empty());
    // an AP's beacon interval of 65535 TU would be 65 s: cut to 10 s, said in an event
    let long = Vif {
        beacon_int: u16::MAX,
        ..vif
    };
    // B4: the probe thread's bound is the 10 s rule, every other thread's MacWaits::short;
    // a bound above 10 s is still cut to 10 s
    let too_long = MacWaits {
        func_sw_ms: 60_000,
        ..MacWaits::probe()
    };
    for (waits, ms) in [
        (MacWaits::probe(), 10_000),
        (MacWaits::short(), 1_001),
        (too_long, 10_000),
    ] {
        let mut l = fake();
        l.on_read(|r| (r == cfg1).then_some(B_AX_PORT_FUNC_EN));
        let mut m = MacState::with_waits(waits);
        port_update(&mut l, &mut m, &long).unwrap();
        assert_eq!(ops(&l, 0)[1], (DELAY, 0, ms * 1000, 0, 0, 0));
        assert_eq!(
            m.events(),
            [MacEvent::WaitClamped {
                what: "port func_sw msleep(beacon_int + 1)",
                want_ms: 65536,
                ms
            }]
        );
    }
    // the short bound still waits a whole beacon of 1000 TU
    let slow = Vif {
        beacon_int: 1000,
        ..vif
    };
    let mut l = fake();
    l.on_read(|r| (r == cfg1).then_some(B_AX_PORT_FUNC_EN));
    let mut m = MacState::new();
    port_update(&mut l, &mut m, &slow).unwrap();
    assert_eq!(ops(&l, 0)[1], (DELAY, 0, 1_001_000, 0, 0, 0));
    assert!(m.events().is_empty());
}

#[test]
fn the_waits_default_short_and_the_probe_takes_linuxs() {
    assert_eq!(MacState::new().waits, MacWaits::short());
    assert_eq!(MacState::default().waits, MacWaits::short());
    assert_eq!(
        MacWaits::probe(),
        MacWaits {
            sch_tx_c2h_us: RTW89_C2H_TIMEOUT,
            func_sw_ms: 10_000
        }
    );
    assert_eq!(Bringup::new().mac.waits, MacWaits::probe());
    // the short C2H wait bounds a frozen clock's steps too
    let (mut b, mut m, mut fw) = sch_fake(RTW89_FWCMD_C2HREG_FUNC_TX_PAUSE_RPT, B_AX_CMAC_EN);
    b.on_read(|a| match a {
        R_AX_CMAC_FUNC_EN => Some(B_AX_CMAC_EN),
        R_AX_C2HREG_CTRL => Some(0),
        _ => None,
    });
    stop_sch_tx(&mut b, &mut m, &mut fw, SchTxSel::All).unwrap();
    let waits = ops(&b, 0).iter().filter(|x| x.0 == DELAY).count();
    assert_eq!(waits as u32, MacWaits::short().sch_tx_c2h_us);
}

#[test]
fn undrained_events_are_capped_and_counted() {
    let mut m = MacState::new();
    m.set_event_cap(3);
    for ms in 0..5 {
        m.push(MacEvent::WaitClamped {
            what: "t",
            want_ms: ms,
            ms,
        });
    }
    let kept: Vec<u32> = m
        .events()
        .iter()
        .map(|e| match e {
            MacEvent::WaitClamped { ms, .. } => *ms,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(kept, [2, 3, 4], "the oldest go");
    assert_eq!(m.events_dropped(), 2);
    assert_eq!(m.take_events().len(), 3);
    assert_eq!(m.events_dropped(), 2, "never reset");
    // the default cap holds a sch TX stop that keeps failing
    let (mut b, mut m, mut fw) = sch_fake(0x55, B_AX_CMAC_EN);
    m.waits.sch_tx_c2h_us = 1;
    for _ in 0..crate::events::DEFAULT_EVENT_CAP + 10 {
        stop_sch_tx(&mut b, &mut m, &mut fw, SchTxSel::All).unwrap();
    }
    assert_eq!(m.events().len(), crate::events::DEFAULT_EVENT_CAP);
    assert_eq!(m.events_dropped(), 10);
}

#[test]
fn ports_past_the_chips_and_running_ap_ports_are_refused() {
    let mut b = fake();
    let e = port_update(&mut b, &mut MacState::new(), &Vif::station(0, 5, [0; 6])).unwrap_err();
    assert!(matches!(e, Rtw89Error::Invalid { value: 5, .. }));
    assert!(b.mem.trace.is_empty(), "refused before any access");
    let mut a = fake();
    a.on_read(|r| (r == R_AX_PORT_CFG_P0).then_some(B_AX_PORT_FUNC_EN));
    let ap = Vif {
        net_type: RTW89_NET_TYPE_AP_MODE,
        ..Vif::station(0, 0, [0; 6])
    };
    let e = port_update(&mut a, &mut MacState::new(), &ap).unwrap_err();
    assert!(matches!(
        e,
        Rtw89Error::Invalid {
            value: RTW89_NET_TYPE_AP_MODE,
            ..
        }
    ));
    assert_eq!(ops(&a, 0), [rd32(R_AX_PORT_CFG_P0)]);
    // port 4: the second BSS color register, its own HIQ window
    let mut f = fake();
    port_update(&mut f, &mut MacState::new(), &Vif::station(0, 4, [0; 6])).unwrap();
    let o = ops(&f, 0);
    assert!(o.contains(&wr(1, R_AX_PORT_HGQ_WINDOW_CFG + 3, 0)));
    assert!(o.contains(&mask32(
        R_AX_PTCL_BSS_COLOR_0 + 4,
        B_AX_BSS_COLOB_AX_PORT_4_MASK,
        0
    )));
    assert!(
        !o.iter().any(|x| x.2 == R_AX_MBSSID_CTRL),
        "cfg_mbssid only for port 0"
    );
}

// ---- rtw89_mac_set_macid_pause (mac.c:4437) with its own H2C (fw.c:5180) ----

fn words(p: &[u8]) -> Vec<u32> {
    p.chunks(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[test]
fn macid_pause_checks_the_cmac_and_sends_the_c_struct() {
    let mut r = run();
    enabled(&mut r.b);
    macid_pause(&mut r.b, &mut r.rings, &mut r.fws, 37, true).unwrap();
    let o = ops(&r.b, 0);
    assert_eq!(o[0], CMAC_EN);
    assert_eq!(o[o.len() - 2].2, R_AX_CH12_TXBD_IDX, "sent on CH12");
    let h2c = &r.b.writes.last().unwrap().1[24..];
    let w0 = u32::from_le_bytes([h2c[0], h2c[1], h2c[2], h2c[3]]);
    let w1 = u32::from_le_bytes([h2c[4], h2c[5], h2c[6], h2c[7]]);
    assert_eq!(
        (
            crate::bus::field(w0, H2C_HDR_CAT),
            crate::bus::field(w0, H2C_HDR_CLASS),
            crate::bus::field(w0, H2C_HDR_FUNC),
            w1 & H2C_HDR_REC_ACK != 0,
            w1 & H2C_HDR_DONE_ACK != 0
        ),
        (
            H2C_CAT_MAC,
            H2C_CL_MAC_FW_OFLD,
            H2C_FUNC_MAC_MACID_PAUSE,
            true,
            false
        )
    );
    // pause_grp[1], mask_grp[1]: MAC ID 37 = group 1, bit 5
    assert_eq!(words(&h2c[8..]), [0, 1 << 5, 0, 0, 0, 1 << 5, 0, 0]);
    macid_pause(&mut r.b, &mut r.rings, &mut r.fws, 2, false).unwrap();
    let h2c = &r.b.writes.last().unwrap().1[32..];
    assert_eq!(words(h2c), [0, 0, 0, 0, 4, 0, 0, 0]);
    // a MAC ID past the struct's 128 is refused, nothing sent
    let n = r.b.writes.len();
    let e = macid_pause(&mut r.b, &mut r.rings, &mut r.fws, 128, true).unwrap_err();
    assert!(matches!(e, Rtw89Error::Invalid { value: 128, .. }));
    assert_eq!(r.b.writes.len(), n);
}

// ---- rtw89_core_start tail (core.c:6699-6721) ----

#[test]
fn the_core_start_tail_runs_radio_on_between_rts_and_the_fw_log() {
    let mut r = run();
    enabled(&mut r.b);
    let order = core::cell::RefCell::new(Vec::new());
    let mut track = || order.borrow_mut().push("track");
    let mut radio = |b: &mut HookBus<'_>, _: &mut PciRings, _: &mut FwState| -> Rtw89Result<()> {
        order.borrow_mut().push("radio");
        b.w32(0xB7C, 1)?;
        Ok(())
    };
    core_start_tail(
        &mut r.b,
        &mut r.rings,
        &mut r.fws,
        Some(&mut track),
        Some(&mut radio),
    )
    .unwrap();
    let o = ops(&r.b, 0);
    assert_eq!(
        o[..6],
        [
            CMAC_EN,
            wr(
                4,
                R_AX_PPDU_STAT,
                B_AX_PPDU_STAT_RPT_EN
                    | B_AX_APP_MAC_INFO_RPT
                    | B_AX_APP_PLCP_HDR_RPT
                    | B_AX_PPDU_STAT_RPT_CRC32
            ),
            mask32(
                R_AX_HW_RPT_FWD,
                B_AX_FWD_PPDU_STAT_MASK,
                RTW89_PRPT_DEST_HOST
            ),
            rmw(
                2,
                R_AX_AGG_LEN_HT_0,
                B_AX_RTS_TXTIME_TH_MASK,
                prep(B_AX_RTS_TXTIME_TH_MASK, 88 >> 5)
            ),
            rmw(
                2,
                R_AX_AGG_LEN_HT_0,
                B_AX_RTS_LEN_TH_MASK,
                prep(B_AX_RTS_LEN_TH_MASK, 255)
            ),
            wr(4, 0xB7C, 1),
        ]
    );
    assert_eq!(*order.borrow(), ["track", "radio"]);
    let log = &r.b.writes.last().unwrap().1[24..];
    let w0 = u32::from_le_bytes([log[0], log[1], log[2], log[3]]);
    assert_eq!(
        crate::bus::field(w0, H2C_HDR_FUNC),
        H2C_FUNC_LOG_CFG,
        "then the fw log H2C"
    );
    assert_eq!(words(&log[12..16]), [0], "log components off");
    let n = r.b.mem.trace.len();
    cfg_ppdu_status(&mut r.b, false).unwrap();
    assert_eq!(
        ops(&r.b, n),
        [CMAC_EN, clr32(R_AX_PPDU_STAT, B_AX_PPDU_STAT_RPT_EN)]
    );
}

// ---- rtw89_mac_check_mac_en_ax (mac.c:62): 0xEAEAEAEA, 0xDEADBEEF, missing bits ----

#[test]
fn check_mac_en_refuses_dead_reads_and_missing_bits() {
    for (sel, v, ok) in [
        (MacSel::Dmac, 0xEAEA_EAEA, false),
        (MacSel::Dmac, B_AX_MAC_FUNC_EN, false),
        (MacSel::Dmac, B_AX_MAC_FUNC_EN | B_AX_DMAC_FUNC_EN, true),
        (MacSel::Cmac, 0xDEAD_BEEF, false),
        (MacSel::Cmac, 0, false),
        (MacSel::Cmac, B_AX_CMAC_EN, true),
    ] {
        let mut b = fake();
        b.on_read(|a| (a == R_AX_DMAC_FUNC_EN || a == R_AX_CMAC_FUNC_EN).then_some(v));
        let r = check_mac_en(&mut b, sel);
        assert_eq!(r.is_ok(), ok, "{sel:?} {v:#x}");
        if let Err(e) = r {
            assert!(matches!(e, Rtw89Error::MacNotEnabled { value, .. } if value == v));
        }
    }
}

// ---- scheduler TX stop/resume (mac.c:3363-3494) ----

const TXEN: u16 = B_AX_CTN_TXEN_ALL_MASK as u16;

/// test-macinit `schFake`: CTN_TXEN 0x1234, the C2H register answers `id`, firmware ready
fn sch_fake<'a>(id: u32, cmac: u32) -> (HookBus<'a>, MacState, FwState) {
    let mut b = fake();
    b.on_read(move |a| match a {
        R_AX_CTN_TXEN => Some(0x1234),
        R_AX_CMAC_FUNC_EN => Some(cmac),
        R_AX_C2HREG_CTRL => Some(1),
        R_AX_C2HREG_DATA0 => Some(id | (2 << 8)),
        _ => None,
    });
    let mut fw = FwState::new();
    fw.ready = true;
    (b, MacState::new(), fw)
}

/// macinit-spec.mjs `SCH_TX_H2C` with tx_en, mask and the counters' new value n
fn sch_tx_h2c(tx_en: u16, mask: u16, n: u32) -> Vec<O> {
    vec![
        (POLL, 1, R_AX_H2CREG_CTRL, 0xff, 0, 5),
        wr(
            4,
            R_AX_H2CREG_DATA0,
            prep(RTW89_H2CREG_SCH_TX_EN_W0_EN, tx_en.into())
                | prep(
                    RTW89_H2CREG_HDR_FUNC_MASK,
                    RTW89_FWCMD_H2CREG_FUNC_SCH_TX_EN,
                )
                | prep(RTW89_H2CREG_HDR_LEN_MASK, 2),
        ),
        wr(
            4,
            R_AX_H2CREG_DATA1,
            prep(RTW89_H2CREG_SCH_TX_EN_W1_MASK, mask.into())
                | prep(RTW89_H2CREG_SCH_TX_EN_W1_BAND, 0),
        ),
        wr(4, R_AX_H2CREG_DATA2, 0),
        wr(4, R_AX_H2CREG_DATA3, 0),
        rmw(
            1,
            R_AX_UDM1 + 1,
            B_AX_UDM1_HALMAC_H2C_DEQ_CNT_MASK >> 8,
            prep(B_AX_UDM1_HALMAC_H2C_DEQ_CNT_MASK >> 8, n),
        ),
        wr(1, R_AX_H2CREG_CTRL, B_AX_H2CREG_TRIGGER),
        (RD, 1, R_AX_C2HREG_CTRL, 0, 0, 0),
        rd32(R_AX_C2HREG_DATA0),
        rd32(R_AX_C2HREG_DATA1),
        rd32(R_AX_C2HREG_DATA2),
        rd32(R_AX_C2HREG_DATA3),
        wr(1, R_AX_C2HREG_CTRL, 0),
        rmw(
            1,
            R_AX_UDM1 + 1,
            B_AX_UDM1_HALMAC_C2H_ENQ_CNT_MASK >> 8,
            prep(B_AX_UDM1_HALMAC_C2H_ENQ_CNT_MASK >> 8, n),
        ),
    ]
}

#[test]
fn stop_and_resume_go_through_the_h2c_registers_once_the_firmware_runs() {
    let (mut b, mut m, mut fw) = sch_fake(RTW89_FWCMD_C2HREG_FUNC_TX_PAUSE_RPT, B_AX_CMAC_EN);
    let tx_en = stop_sch_tx(&mut b, &mut m, &mut fw, SchTxSel::All).unwrap();
    assert_eq!(tx_en, 0x1234);
    let mut want = vec![(RD, 2, R_AX_CTN_TXEN, 0, 0, 0), CMAC_EN];
    want.extend(sch_tx_h2c(0, TXEN, 1));
    assert_eq!(ops(&b, 0), want);
    assert_eq!(m.sch_tx_err, None);
    let n = b.mem.trace.len();
    resume_sch_tx(&mut b, &mut m, &mut fw, tx_en).unwrap();
    let mut want = vec![CMAC_EN];
    want.extend(sch_tx_h2c(0x1234, TXEN, 2));
    assert_eq!(ops(&b, n), want);
    // every rtw89_sch_tx_sel, also by Linux's enum value
    for (v, mask) in [
        (RTW89_SCH_TX_SEL_ALL, B_AX_CTN_TXEN_ALL_MASK),
        (RTW89_SCH_TX_SEL_HIQ, B_AX_CTN_TXEN_HGQ),
        (RTW89_SCH_TX_SEL_MG0, B_AX_CTN_TXEN_MGQ),
        (RTW89_SCH_TX_SEL_MACID, B_AX_CTN_TXEN_ALL_MASK),
    ] {
        let sel = SchTxSel::from_linux(v).unwrap();
        let (mut s, mut m, mut fw) = sch_fake(RTW89_FWCMD_C2HREG_FUNC_TX_PAUSE_RPT, B_AX_CMAC_EN);
        assert_eq!(stop_sch_tx(&mut s, &mut m, &mut fw, sel).unwrap(), 0x1234);
        let mut want = vec![(RD, 2, R_AX_CTN_TXEN, 0, 0, 0), CMAC_EN];
        want.extend(sch_tx_h2c(0, mask as u16, 1));
        assert_eq!(ops(&s, 0), want, "{sel:?}");
    }
    assert_eq!(SchTxSel::from_linux(4), None, "an unknown sel is no sel");
}

#[test]
fn sch_tx_errors_linux_ignores_are_kept_and_the_caller_goes_on() {
    // a wrong C2H function: tx_en still comes back, the error is kept and logged
    let (mut w, mut m, mut fw) = sch_fake(RTW89_FWCMD_C2HREG_FUNC_NULL, B_AX_CMAC_EN);
    assert_eq!(
        stop_sch_tx(&mut w, &mut m, &mut fw, SchTxSel::All).unwrap(),
        0x1234
    );
    assert!(matches!(
        m.sch_tx_err,
        Some(Rtw89Error::C2hRegAnswer {
            reg: R_AX_C2HREG_DATA0,
            ..
        })
    ));
    assert!(m.event_lines()[0].contains("stop sch tx failed"));
    // the H2C register never free
    let (mut b, mut m, mut fw) = sch_fake(RTW89_FWCMD_C2HREG_FUNC_TX_PAUSE_RPT, B_AX_CMAC_EN);
    b.on_poll(|a, _| Ok(a != R_AX_H2CREG_CTRL));
    assert_eq!(
        stop_sch_tx(&mut b, &mut m, &mut fw, SchTxSel::All).unwrap(),
        0x1234
    );
    assert!(matches!(m.sch_tx_err, Some(Rtw89Error::H2cRegBusy { .. })));
    // CMAC off: only the check
    let (mut c, mut m, mut fw) = sch_fake(RTW89_FWCMD_C2HREG_FUNC_TX_PAUSE_RPT, 0xDEAD_BEEF);
    resume_sch_tx(&mut c, &mut m, &mut fw, 0x1234).unwrap();
    assert!(matches!(
        m.sch_tx_err,
        Some(Rtw89Error::MacNotEnabled { .. })
    ));
    assert_eq!(ops(&c, 0), [CMAC_EN]);
    // a lost bus is not swallowed
    let (mut g, mut m, mut fw) = sch_fake(RTW89_FWCMD_C2HREG_FUNC_TX_PAUSE_RPT, B_AX_CMAC_EN);
    g.on_poll(|_, _| Err(BusError::Gone));
    let e = stop_sch_tx(&mut g, &mut m, &mut fw, SchTxSel::All).unwrap_err();
    assert_eq!(e, Rtw89Error::Bus(BusError::Gone));
}

#[test]
fn without_the_firmware_the_register_itself_is_changed() {
    let (mut n, mut m, mut fw) = sch_fake(RTW89_FWCMD_C2HREG_FUNC_TX_PAUSE_RPT, B_AX_CMAC_EN);
    mac_pwr_off(&mut n, 1, &mut m, &mut fw).unwrap();
    let t = n.mem.trace.len();
    resume_sch_tx(&mut n, &mut m, &mut fw, 0x1234).unwrap();
    assert_eq!(
        ops(&n, t),
        [
            CMAC_EN,
            rmw(2, R_AX_CTN_TXEN, TXEN.into(), 0x1234 & u32::from(TXEN))
        ]
    );
    let t = n.mem.trace.len();
    assert_eq!(
        stop_sch_tx(&mut n, &mut m, &mut fw, SchTxSel::Hiq).unwrap(),
        0x1234
    );
    assert_eq!(
        ops(&n, t),
        [
            (RD, 2, R_AX_CTN_TXEN, 0, 0, 0),
            CMAC_EN,
            rmw(2, R_AX_CTN_TXEN, B_AX_CTN_TXEN_HGQ, 0)
        ]
    );
}

#[test]
fn a_frozen_clock_cannot_make_the_sch_tx_answer_wait_endless() {
    let (mut b, mut m, mut fw) = sch_fake(RTW89_FWCMD_C2HREG_FUNC_TX_PAUSE_RPT, B_AX_CMAC_EN);
    b.on_read(|a| match a {
        R_AX_CTN_TXEN => Some(0x1234),
        R_AX_CMAC_FUNC_EN => Some(B_AX_CMAC_EN),
        R_AX_C2HREG_CTRL => Some(0),
        _ => None,
    });
    m.waits.sch_tx_c2h_us = 500;
    stop_sch_tx(&mut b, &mut m, &mut fw, SchTxSel::All).unwrap();
    assert!(matches!(
        m.sch_tx_err,
        Some(Rtw89Error::C2hRegTimeout { .. })
    ));
    let waits = ops(&b, 0).iter().filter(|x| x.0 == DELAY).count();
    assert_eq!(waits, 500, "the caller's timeout, in 1 us steps");
}

// ---- rtw89_ops_configure_filter (mac80211.c:308) with rtw89_mac_set_rx_fltr (mac.c:2678) ----

#[test]
fn every_filter_flag_changes_hal_rx_fltr_as_linux() {
    let d = 0x0300_44BEu32;
    assert_eq!(default_rx_fltr(), d, "DEFAULT_AX_RX_FLTR");
    let a = |bits: u32| bits;
    let not = |v: u32, m: u32| v & !m;
    let scan = B_AX_A_BCN_CHK_EN | B_AX_A_BC | B_AX_A_A1_MATCH;
    let f = FilterFlags::default;
    let steps = [
        (
            FilterFlags {
                allmulti: true,
                ..f()
            },
            false,
            not(d, a(B_AX_A_MC)),
        ),
        (f(), false, d),
        (
            FilterFlags {
                fcsfail: true,
                ..f()
            },
            false,
            d | B_AX_A_CRC32_ERR,
        ),
        (
            FilterFlags {
                other_bss: true,
                ..f()
            },
            false,
            not(d, B_AX_A_A1_MATCH),
        ),
        (
            FilterFlags {
                probe_req: true,
                ..f()
            },
            false,
            not(d, B_AX_A_BC_CAM_MATCH | B_AX_A_UC_CAM_MATCH),
        ),
        (
            FilterFlags {
                bcn_prbresp_promisc: true,
                ..f()
            },
            true,
            not(d, scan),
        ),
        (f(), true, d),
        (f(), false, d),
    ];
    let mut b = fake();
    let mut m = MacState::new();
    m.rx_fltr = d | B_AX_SNIFFER_MODE;
    for (flags, scanning, hal) in steps {
        let t = b.mem.trace.len();
        let rx = if scanning { not(hal, scan) } else { hal };
        assert_eq!(
            rx_filter(&mut b, &mut m, flags, scanning).unwrap(),
            rx,
            "{flags:?}"
        );
        assert_eq!(m.rx_fltr, hal, "hal.rx_fltr after {flags:?}");
        let cfg = !B_AX_RX_MPDU_MAX_LEN_MASK;
        assert_eq!(ops(&b, t), [rmw(4, R_AX_RX_FLTR_OPT, cfg, rx & cfg)]);
    }
}

// ---- the whole stage mac against bringup-golden.txt ----

/// bringup-golden.txt: the look after the download ends here, then the efuse dump and
/// phycap (phy, not ported: replayed), the probe's end at 6657, stage mac 6699-7810
const FW_LINES: usize = 1149;
const PHY_EFUSE_END: usize = 6656;
const MAC_LINES: usize = 7810;

/// t2-fw.mjs `fakeDev`'s answers (as fw/tests.rs `t2_chip`) plus what the probe's end and
/// stage mac read: the PCIe capability, the radio switch on, all public pages free
pub(super) fn golden_chip() -> MemBus {
    let mut b = MemBus::new();
    for (off, v) in [
        (0x04u16, 0x0010_0002u32),
        (0x34, 0x70),
        (0x70, 0x0002_0010),
        (0x80, 0x1011_0000),
        (0x98, 0x0400), // DEVCTL2: LTR enabled, as measured (spec/wlan.md §2.1)
    ] {
        b.cfg.insert(off, v);
    }
    b.forced.insert(R_AX_SYS_CFG1, prep(B_AX_CHIP_VER_MASK, 1));
    b.forced
        .insert(R_AX_WLAN_XTAL_SI_CTRL, prep(B_AX_WL_XTAL_SI_DATA_MASK, 1));
    b.forced.insert(R_AX_EFUSE_CTRL, B_AX_EF_RDY | 0xff);
    b.forced.insert(R_AX_GPIO_EXT_CTRL, B_AX_GPIO_IN_9);
    b.forced.insert(R_AX_RESPBA_CAM_CTRL, 0);
    b.forced.insert(R_AX_PUB_PAGE_INFO2, 446);
    b
}

/// The PCI status half of dword 4 is read-only on a chip; the memory bus stores the
/// command register's writes as whole dwords, so the capability-list bit is put back
pub(super) fn status_cap_list(b: &mut MemBus) {
    *b.cfg.entry(4).or_default() |= 0x0010_0000;
}

/// Stage fw with the probe's end, then stage mac, on `bus`; the efuse/phycap lines (phy's)
/// come from the golden file as they are
fn stage_fw_and_mac(bus: &mut MemBus, gold: &[String]) -> Bringup {
    let mut run = Bringup::new();
    run.probe_power_on(bus, &RingConfig::js()).unwrap();
    let fw = fw_file();
    run.partial_init(bus, &fw).unwrap();
    run.fwdl_look(bus).unwrap();
    for l in &gold[FW_LINES..PHY_EFUSE_END] {
        let op: Op = trace::parse_line(l).unwrap_or_else(|| panic!("not an op: {l}"));
        trace::replay(bus, &op).unwrap();
    }
    // macSetupPhycap's register H2C/C2H moved the counters (lines 6643-6656)
    run.fw.h2c_counter = 1;
    run.fw.c2h_counter = 1;
    status_cap_list(bus);
    run.probe_done(bus, OPTS.rfe).unwrap();
    run.probe_end(bus).unwrap();
    run.core_start_mac(bus, &fw, &OPTS).unwrap();
    run
}

#[test]
fn stage_mac_is_golden_on_the_memory_bus() {
    let gold = golden();
    assert_eq!(gold[FW_LINES - 1], "# Linux again");
    assert_eq!(
        gold[PHY_EFUSE_END], "rmw32 0x00000074 0x00000020 0x00000000",
        "pre-deinit"
    );
    assert_eq!(gold[6698], "# stage mac");
    assert_eq!(gold[MAC_LINES - 1], "# Linux again");
    let mut b = golden_chip();
    let mut run = stage_fw_and_mac(&mut b, &gold);
    let ours = b.lines();
    if let Err(m) = trace::compare_prefix(&ours, &gold) {
        panic!(
            "differs from bringup-golden.txt at line {}: {:?} vs {:?}",
            m.line + 1,
            m.ours,
            m.golden
        );
    }
    assert_eq!(ours.len(), MAC_LINES, "trace length");
    assert!(run.fw.ready && run.fw.tries == 1 && run.fw.pieces == 164);
    assert_eq!(run.mac.power_on, Some(true));
    assert!(run.core && run.tx.is_some());
    assert!(run.mac.events().is_empty(), "{:?}", run.mac.events());
    // the stop after the core ran: DMA off (Linux's rtw89_hci_deinit), power off, ring reset
    run.stop(&mut b).unwrap();
    let tail = &b.lines()[MAC_LINES..];
    assert_eq!(tail[0], "# stop");
    assert!(
        !tail[1].starts_with("# not Linux"),
        "rtw89_hci_deinit, as Linux"
    );
    assert!(!run.fw.ready);
    assert_eq!(run.mac.power_on, Some(false));
    assert!(b.dma_live.is_empty(), "the rings' DMA regions given back");
}

#[test]
fn a_blocked_radio_stops_at_the_probes_end() {
    let gold = golden();
    let mut b = golden_chip();
    b.forced.insert(R_AX_GPIO_EXT_CTRL, 0);
    let mut run = Bringup::new();
    run.probe_power_on(&mut b, &RingConfig::js()).unwrap();
    let fw = fw_file();
    run.partial_init(&mut b, &fw).unwrap();
    run.fwdl_look(&mut b).unwrap();
    for l in &gold[FW_LINES..PHY_EFUSE_END] {
        trace::replay(&mut b, &trace::parse_line(l).unwrap()).unwrap();
    }
    status_cap_list(&mut b);
    run.probe_done(&mut b, 1).unwrap();
    let e = run.probe_end(&mut b).unwrap_err();
    assert_eq!(
        e,
        Rtw89Error::RadioBlocked {
            reg: R_AX_GPIO_EXT_CTRL,
            value: 0
        }
    );
    assert_eq!(b.lines().len(), 6698, "up to the rfkill read");
    run.stop(&mut b).unwrap();
    assert!(
        !b.lines()[6698..]
            .iter()
            .any(|l| l.starts_with("poll32 0x00000004 0x00000200")),
        "already powered off at the probe's end: no second power-off"
    );
}

#[test]
fn golden_lines_parse_back_into_the_ops_they_print() {
    for l in golden().iter().filter(|l| !l.starts_with('#')) {
        let op = trace::parse_line(l).unwrap_or_else(|| panic!("{l}"));
        assert_eq!(&op.line(), l);
    }
    assert_eq!(trace::parse_line("# stage mac"), None);
    assert_eq!(trace::parse_line("rd32 0x1"), None);
    assert_eq!(trace::parse_line("bogus 0x1 0x2 0x3"), None);
}

// ---- the MAC part of the later stages, where bringup-golden.txt shows it ----

/// 1-based golden lines `from..=to`
fn window(gold: &[String], from: usize, to: usize) -> &[String] {
    &gold[from - 1..to]
}

/// Ours against a window of the golden file, line by line
fn same_as(ours: &[String], want: &[String], what: &str) {
    for (i, (o, w)) in ours.iter().zip(want).enumerate() {
        assert_eq!(o, w, "{what}: line {i} of the window");
    }
    assert_eq!(ours.len(), want.len(), "{what}: as many lines");
}

/// A chip whose CMAC reads as enabled, the rings as the JS lays them out (CH12 from slot 0)
fn window_chip() -> (MemBus, PciRings) {
    let mut b = MemBus::new();
    b.forced.insert(R_AX_CMAC_FUNC_EN, B_AX_CMAC_EN);
    let rings = pci::pci_alloc(&mut b, &RingConfig::js()).unwrap();
    b.trace.clear();
    (b, rings)
}

/// sta's vif hooks for the golden window: the MACID pause H2C is macinit's own; the
/// others (sta's, WL-3) send nothing here, the window ends after the pause H2C
struct PauseOnly<'r> {
    rings: &'r mut PciRings,
    fws: &'r mut FwState,
}

impl VifHooks<MemBus> for PauseOnly<'_> {
    fn macid_pause(&mut self, b: &mut MemBus, sh: u32, grp: u32, p: bool) -> Rtw89Result<()> {
        h2c::h2c_macid_pause(b, self.rings, self.fws, sh, grp, p).map(|_| ())
    }
    fn role_maintain(&mut self, _: &mut MemBus, _: u32) -> Rtw89Result<()> {
        Ok(())
    }
    fn join_info(&mut self, _: &mut MemBus, _: bool) -> Rtw89Result<()> {
        Ok(())
    }
    fn cam_init(&mut self, _: &mut MemBus) -> Rtw89Result<()> {
        Ok(())
    }
    fn addr_cam(&mut self, _: &mut MemBus, _: u32) -> Rtw89Result<()> {
        Ok(())
    }
    fn default_cmac_tbl(&mut self, _: &mut MemBus) -> Rtw89Result<()> {
        Ok(())
    }
}

#[test]
fn the_core_start_tail_and_vif_init_are_golden_where_the_trace_has_them() {
    let gold = golden();
    // stage phy's end: rtw89_core_start after rtw89_phy_dm_init (15540-15605), the radio-on
    // lines between (btc's, replayed), then stage chan's add_interface: rtw89_mac_vif_init
    // up to the MACID pause H2C (15607-15652). CH12 runs from slot 0 there.
    assert_eq!(gold[15540 - 1], "rd32 0x0000C000 0x00000000 0x00000000");
    assert_eq!(
        gold[15545 - 1],
        "rmw8 0x00000040 0x00000000 0x00000020",
        "btc radio on"
    );
    assert_eq!(
        gold[15602 - 1],
        "mwr32 0x00629B00 0x00000000 0x4000002C",
        "fw log H2C"
    );
    assert_eq!(gold[15606 - 1], "# stage chan");
    assert_eq!(
        gold[15649 - 1],
        "mwr32 0x00629B08 0x00000000 0x40000040",
        "MACID pause H2C"
    );
    let (mut b, mut rings) = window_chip();
    let mut fws = FwState::new();
    let radio: Vec<Op> = window(&gold, 15545, 15601)
        .iter()
        .map(|l| trace::parse_line(l).unwrap())
        .collect();
    let mut radio_on = |bus: &mut MemBus, _: &mut PciRings, _: &mut FwState| -> Rtw89Result<()> {
        for op in &radio {
            trace::replay(bus, op)?;
        }
        Ok(())
    };
    core_start_tail(&mut b, &mut rings, &mut fws, None, Some(&mut radio_on)).unwrap();
    same_as(&b.lines(), window(&gold, 15540, 15605), "core start tail");
    b.trace.clear();
    let vif = Vif::station(0, 0, [0x02, 0xb1, 0x75, 0x00, 0x00, 0x01]);
    let mut m = MacState::new();
    let mut hooks = PauseOnly {
        rings: &mut rings,
        fws: &mut fws,
    };
    vif_init(&mut b, &mut m, &vif, &mut hooks).unwrap();
    same_as(&b.lines(), window(&gold, 15607, 15652), "vif init");
    let mem = |a: u32| {
        (0..4).fold(0u32, |v, i| {
            v | u32::from(b.mem.get(&(a + i)).copied().unwrap_or(0)) << (8 * i)
        })
    };
    // the pause H2C's payload after WD (24) and H2C header (8): mask_grp[0] = BIT(0)
    let buf = rings.tx("CH12").unwrap().bufs[1];
    let p: Vec<u32> = (0..8).map(|i| mem(buf + 32 + 4 * i)).collect();
    assert_eq!(p, [0, 0, 0, 0, 1, 0, 0, 0]);
}

#[test]
fn stop_sch_tx_and_the_rx_filter_are_golden_where_the_trace_has_them() {
    let gold = golden();
    // stage chan: rtw89_chip_set_channel_prepare stops the scheduler (the third register
    // H2C of the run: counter 3), then chan's cfg_ppdu_status(false)
    assert_eq!(gold[15701 - 1], "rd16 0x0000C348 0x00000000 0x00000000");
    let (mut b, _) = window_chip();
    b.forced.insert(R_AX_C2HREG_CTRL, 1);
    b.forced.insert(
        R_AX_C2HREG_DATA0,
        RTW89_FWCMD_C2HREG_FUNC_TX_PAUSE_RPT | (2 << 8),
    );
    let mut m = MacState::new();
    let mut fws = FwState::new();
    fws.ready = true;
    fws.h2c_counter = 2;
    fws.c2h_counter = 2;
    stop_sch_tx(&mut b, &mut m, &mut fws, SchTxSel::All).unwrap();
    cfg_ppdu_status(&mut b, false).unwrap();
    same_as(
        &b.lines(),
        window(&gold, 15701, 15718),
        "stop sch tx, PPDU status off",
    );
    assert_eq!(m.sch_tx_err, None);
    // rtw89_ops_configure_filter: the interface's first filter (16255), then the scan's
    // (bcn_prbresp_promisc while scanning, 16299)
    b.trace.clear();
    rx_filter(&mut b, &mut m, FilterFlags::default(), false).unwrap();
    let scan = FilterFlags {
        bcn_prbresp_promisc: true,
        ..FilterFlags::default()
    };
    rx_filter(&mut b, &mut m, scan, true).unwrap();
    let l = b.lines();
    assert_eq!(l[0], gold[16255 - 1]);
    assert_eq!(l[1], gold[16299 - 1]);
}
