//! rtw89_track_work (core.c:5503) on V1's fake chip, connected to its AP (js/test-track.mjs
//! ported): the parts in Linux's order, the values DIG, EDCCA, UL TB and the TX path diversity
//! write (computed here from phy.c's formulas, as the JS test does), the traffic statistics and
//! their level, the RX interrupt mitigation, the work's timing, the scan, the rfkill poll. Where
//! the JS spied on the calls, this test reads the trace and btc's notification counter.

use rtw89_core::bus::prep;
use rtw89_core::phy::io::BB;
use rtw89_core::sta::StaEvent;
use rtw89_core::sta::scan::{self, ScanReq};
use rtw89_core::sta::track::{self, Accu, TRACK_MS};
use rtw89_core::tables::regs::*;

use super::life::{CCTL, Life};
use super::*;

const TP: (u32, u32, u32) = (H2C_CAT_MAC, H2C_CL_MAC_FW_OFLD, H2C_FUNC_OFLD_TP);
const RA: (u32, u32, u32) = (
    H2C_CAT_OUTSRC,
    H2C_CL_OUTSRC_RA,
    H2C_FUNC_OUTSRC_RA_MACIDCFG,
);

fn rmw(addr: u32, mask: u32, v: u32) -> String {
    format!("rmw32 0x{addr:08X} 0x{mask:08X} 0x{:08X}", prep(mask, v))
}

fn run(l: &mut Life) -> (Vec<String>, Vec<super::life::Seen>) {
    let m = l.mark();
    let now = l.env.now;
    l.go(|b, c, s, _| track::track_work(b, c, s, now)).unwrap();
    l.fake.bus.sync().unwrap();
    (l.lines(m), l.h2cs_all(m))
}

#[test]
fn track_work_on_the_fake_ap() {
    if skip("track_work_on_the_fake_ap") {
        return;
    }
    let mut l = Life::new("track");
    let own = l.own();
    l.go(|b, c, s, e| rtw89_core::sta::add_interface(b, c, s, e, own))
        .unwrap();
    let req = ScanReq::new(Band::B2g, &[6], 20);
    let list = l.go(|b, c, s, e| scan::scan(b, c, s, e, &req)).unwrap();
    let bss = list[0].clone();
    l.connect(&bss).unwrap();
    assert!(
        l.sta.track.due.is_some(),
        "track work queued by the core start"
    );
    let rssi_min = l.sta.ap.as_ref().unwrap().avg_rssi.read();

    // one run, associated, idle: the TP offload first, the rfkill poll last
    let (ops, h2cs) = run(&mut l);
    let kinds: Vec<(u32, u32, u32)> = h2cs.iter().map(|s| (s.cat, s.cls, s.func)).collect();
    assert_eq!(
        kinds,
        [TP, RA],
        "the TP offload of the associated vif, the RA update"
    );
    let tp = &h2cs[0];
    assert_eq!(
        (tp.payload.len(), tp.dw(0) & 0xff),
        (4, l.sta.vif.mac_id),
        "TP offload H2C"
    );
    assert_eq!(
        ops.last().unwrap(),
        &format!("rd8 0x{R_AX_GPIO_EXT_CTRL:08X} 0x00000000 0x00000000"),
        "rfkill poll last"
    );
    assert_eq!(
        l.sta.track.due,
        Some(l.env.now + TRACK_MS),
        "the next run queued 2 s on"
    );
    // the environment monitor: no IFS-CLM report on the fake; a new 1900 ms period and the trigger
    let ccx: Vec<&String> = ops
        .iter()
        .filter(|x| {
            x.starts_with(&format!("rmw32 0x{:08X}", BB + R_IFS_COUNTER))
                || x.starts_with(&format!("rmw32 0x{:08X}", BB + R_CCX))
        })
        .collect();
    let want = [
        rmw(BB + R_IFS_COUNTER, B_IFS_CLM_PERIOD_MSK, (1900 * 250) >> 3),
        rmw(
            BB + R_IFS_COUNTER,
            B_IFS_CLM_COUNTER_UNIT_MSK,
            RTW89_CCX_32_US,
        ),
        rmw(BB + R_IFS_COUNTER, B_IFS_COUNTER_CLR_MSK, 0),
        rmw(BB + R_CCX, B_MEASUREMENT_TRIG_MSK, 0),
        rmw(BB + R_IFS_COUNTER, B_IFS_COUNTER_CLR_MSK, 1),
        rmw(BB + R_CCX, B_MEASUREMENT_TRIG_MSK, 1),
    ];
    assert_eq!(
        ccx,
        want.iter().collect::<Vec<_>>(),
        "CCX: period, unit, trigger"
    );
    for r in [R_IFSCNT, R_CLM_EDCCA_RDY] {
        assert!(
            ops.iter()
                .any(|x| x.starts_with(&format!("rd32 0x{:08X}", BB + r))),
            "CCX read {r:#x}"
        );
    }
    // the scan of the join set bypass_dig (rtw89_core_scan_complete): this round's DIG did nothing
    let pd = format!("rmw32 0x{:08X}", BB + R_SEG0R_PD_V1);
    assert!(
        !ops.iter().any(|x| x.starts_with(&pd)) && !l.run.phy.dig.bypass_dig,
        "DIG skipped once after the scan"
    );
    // EDCCA (rtw89_phy_edcca_get_thre_by_rssi): rssi_min >> 1 - 110 + 128 - 3, at least 66
    let th = (((rssi_min >> 1) + EDCCA_UNIT_CONVER - RSSI_UNIT_CONVER - EDCCA_TH_REF) & 0xff)
        .max(EDCCA_TH_L2H_LB);
    let ed: Vec<&String> = ops
        .iter()
        .filter(|x| x.contains(&format!("0x{:08X}", BB + R_SEG0R_EDCCA_LVL_V1)))
        .collect();
    let want: Vec<String> = [B_EDCCA_LVL_MSK0, B_EDCCA_LVL_MSK1, B_EDCCA_LVL_MSK3]
        .map(|m| rmw(BB + R_SEG0R_EDCCA_LVL_V1, m, th))
        .to_vec();
    assert_eq!(ed, want.iter().collect::<Vec<_>>(), "EDCCA threshold {th}");
    // UL TB: one associated station without trigger frames: the default triangle shape
    let tri = l.sta.vif.def_tri_idx;
    assert!(
        ops.contains(&rmw(BB + R_DCFO_OPT, B_TXSHAPE_TRIANGULAR_CFG, tri)),
        "UL TB: TX triangle back to its default"
    );

    // DIG on the first connected round (rtw89_phy_dig_update_para, linked, 2 GHz)
    let (ops, _) = run(&mut l);
    let igi_rssi = (rssi_min >> 1) & 0xff;
    let igi_min = igi_rssi.saturating_sub(10);
    let igi_max = (igi_min + 25).min(0x5a);
    let igi_floor = igi_min.max(0xc);
    let igi_fa = igi_rssi.clamp(igi_floor, igi_max).min(igi_max);
    let under = 16 + PD_TH_SB_FLTR_CMP_VAL + PD_TH_BW20_CMP_VAL;
    let ofdm_th = igi_fa
        .min(igi_rssi)
        .clamp(PD_TH_MIN_RSSI + under, PD_TH_MAX_RSSI + under);
    let pdv = (ofdm_th - under - PD_TH_MIN_RSSI) >> 1;
    let cck = (((igi_fa.min(igi_rssi) as i32 - under as i32) << 24) >> 24).max(CCKPD_TH_MIN_RSSI);
    let dig: Vec<&String> = ops
        .iter()
        .filter(|x| {
            [R_SEG0R_PD_V1, R_BMODE_PDTH_EN_V1, R_BMODE_PDTH_V1]
                .iter()
                .any(|r| x.starts_with(&format!("rmw32 0x{:08X}", BB + r)))
        })
        .collect();
    let want = [
        rmw(BB + R_SEG0R_PD_V1, B_SEG0R_PD_LOWER_BOUND_MSK, pdv),
        rmw(BB + R_SEG0R_PD_V1, B_SEG0R_PD_SPATIAL_REUSE_EN_MSK_V1, 1),
        rmw(BB + R_BMODE_PDTH_EN_V1, B_BMODE_PDTH_LIMIT_EN_MSK_V1, 1),
        rmw(
            BB + R_BMODE_PDTH_V1,
            B_BMODE_PDTH_LOWER_BOUND_MSK_V1,
            (cck - IGI_RSSI_MAX as i32) as u32,
        ),
    ];
    assert_eq!(
        dig,
        want.iter().collect::<Vec<_>>(),
        "DIG: PD lower bound {pdv} for RSSI {rssi_min}, CCK PD"
    );
    assert_eq!(
        (l.run.phy.dig.fa_th, l.run.phy.dig.is_linked_pre),
        ([22, 44, 66, 88], true),
        "fa_th_2g"
    );
    let sdagc = rmw(
        BB + R_PATH0_P20_FOLLOW_BY_PAGCUGC_V2,
        B_PATH0_P20_FOLLOW_BY_PAGCUGC_EN_MSK,
        u32::from(igi_fa > igi_rssi),
    );
    assert!(
        ops.contains(&sdagc),
        "SDAGC follows PAGC only above the PD max"
    );
    assert!(
        !ops.iter()
            .any(|x| x.contains(&format!("0x{:08X}", BB + R_SEG0R_EDCCA_LVL_V1))),
        "EDCCA: no write without a new threshold"
    );

    // traffic: rtw89_traffic_stats_accu / _calc, the levels, rtw89_pci_recalc_int_mit
    let mut f = vec![0x08, 0x01, 0, 0];
    f.extend_from_slice(&l.sta.vif.bssid);
    f.extend_from_slice(&own);
    f.extend_from_slice(&l.sta.vif.bssid);
    f.extend_from_slice(&[0; 10]);
    for _ in 0..100 {
        track::traffic_accu(
            &mut l.sta,
            &f,
            200_000,
            Accu {
                tx: true,
                dev: true,
                vif: true,
            },
        );
    }
    let mut g = f.clone();
    g[4..10].fill(0xff);
    track::traffic_accu(
        &mut l.sta,
        &g,
        200_000,
        Accu {
            tx: true,
            dev: true,
            vif: true,
        },
    );
    assert_eq!(
        (l.sta.track.stats.tx_cnt, l.sta.track.stats.tx_unicast),
        (100, 20_000_000),
        "unicast data counted, group frames not"
    );
    let n0 = l.bst.dm.cnt_notify_wl_sta;
    let (ops, _) = run(&mut l);
    let mit = B_AX_RXMIT_RXP2_SEL
        | B_AX_RXMIT_RXP1_SEL
        | prep(B_AX_RXCOUNTER_MATCH_MASK, RTW89_PCI_RXBD_NUM_MAX / 2)
        | prep(B_AX_RXTIMER_UNIT_MASK, AX_RXTIMER_UNIT_64US)
        | prep(B_AX_RXTIMER_MATCH_MASK, 2048 / 64);
    let st = l.sta.track.stats;
    assert_eq!(
        (st.tx_throughput, st.tx_tfc_lv),
        (20_000_000 / 262_144, RTW89_TFC_HIGH),
        "EWMA throughput, level HIGH"
    );
    assert!(
        ops.contains(&format!(
            "wr32 0x{R_AX_INT_MIT_RX:08X} 0x00000000 0x{mit:08X}"
        )),
        "RX interrupt mitigation on"
    );
    assert!(
        l.bst.dm.cnt_notify_wl_sta > n0 && l.bst.wl.status.busy == 1,
        "level changed: coexistence told, WLAN busy"
    );
    let (ops, _) = run(&mut l);
    assert_eq!(
        l.sta.track.stats.tx_throughput,
        (20_000_000 / 262_144) / 2,
        "EWMA (weight 1/2): half the last throughput"
    );
    assert!(
        ops.contains(&format!(
            "wr32 0x{R_AX_INT_MIT_RX:08X} 0x00000000 0x00000000"
        )),
        "idle: mitigation off"
    );
    // 99 frames are no traffic level (RTW89_TFC_INTERVAL_2SEC: 100 packets)
    for _ in 0..99 {
        track::traffic_accu(
            &mut l.sta,
            &f,
            200_000,
            Accu {
                tx: true,
                dev: true,
                vif: true,
            },
        );
    }
    run(&mut l);
    assert_eq!(
        l.sta.track.stats.tx_tfc_lv, RTW89_TFC_IDLE,
        "99 frames: idle"
    );
    // the AP's RSSI moved by more than 3 dB (3 << RSSI_FACTOR raw): the coexistence hears of it
    let cur = l.sta.ap.as_ref().unwrap().avg_rssi.read();
    l.sta.ap.as_mut().unwrap().prev_rssi = cur - 7;
    let n0 = l.bst.dm.cnt_notify_wl_sta;
    run(&mut l);
    assert!(
        l.bst.dm.cnt_notify_wl_sta > n0 && l.sta.ap.as_ref().unwrap().prev_rssi == cur,
        "RSSI change: coexistence notified"
    );
    l.sta.ap.as_mut().unwrap().prev_rssi = cur - 6;
    let n0 = l.bst.dm.cnt_notify_wl_sta;
    run(&mut l);
    assert_eq!(
        l.bst.dm.cnt_notify_wl_sta, n0,
        "3 dB exactly: no notification"
    );

    // timing: not due, due; while scanning only the traffic statistics; after a scan DIG skips one
    let m = l.mark();
    let now = l.env.now;
    l.go(|b, c, s, _| track::track_due(b, c, s, now)).unwrap();
    assert_eq!(l.fake.bus.lines().len(), m.1, "not due yet: nothing");
    l.env.now += TRACK_MS;
    let now = l.env.now;
    l.go(|b, c, s, _| track::track_due(b, c, s, now)).unwrap();
    assert!(
        l.fake.bus.lines().len() > m.1 && l.sta.track.due == Some(now + TRACK_MS),
        "due: one run, queued again"
    );
    l.sta.scanning = true;
    let (_, h) = run(&mut l);
    l.sta.scanning = false;
    let kinds: Vec<(u32, u32, u32)> = h.iter().map(|s| (s.cat, s.cls, s.func)).collect();
    assert_eq!(
        kinds,
        [TP],
        "scanning: only the traffic statistics and the TP offload"
    );
    l.run.phy.dig.bypass_dig = true; // rtw89_core_scan_complete
    let (ops, _) = run(&mut l);
    assert!(
        !ops.iter().any(|x| x.starts_with(&pd)) && !l.run.phy.dig.bypass_dig,
        "after a scan DIG skips one round"
    );
    // the rfkill poll: a blocked switch is logged once
    assert_eq!(l.ask("dev-set rfkillBlocked 1"), "ok");
    l.sta.take_events();
    run(&mut l);
    assert_eq!(l.ask("dev-set rfkillBlocked 0"), "ok");
    assert!(
        l.sta.track.rfkill
            && l.sta
                .take_events()
                .contains(&StaEvent::Rfkill { blocked: true }),
        "rfkill change logged"
    );
    run(&mut l);
    assert!(!l.sta.track.rfkill);

    // TX path diversity (one TX stream for two antennas): path A much stronger -> RF_A
    l.run.phy.hal.tx_path_diversity = true;
    l.run.phy.hal.antenna_tx = RF_B;
    {
        let ap = l.sta.ap.as_mut().unwrap();
        ap.rssi[0].internal = 100 << 10;
        ap.rssi[1].internal = 80 << 10;
    }
    let (ops, h) = run(&mut l);
    let mux: Vec<&String> = ops
        .iter()
        .filter(|x| {
            [R_P0_RFMODE, R_P1_RFMODE]
                .iter()
                .any(|r| x.starts_with(&format!("rmw32 0x{:08X}", BB + r)))
        })
        .collect();
    let want = [
        rmw(BB + R_P0_RFMODE, B_P0_RFMODE_MUX, 0x12),
        rmw(BB + R_P1_RFMODE, B_P1_RFMODE_MUX, 0x11),
    ];
    assert_eq!(
        (
            l.run.phy.hal.antenna_tx,
            mux,
            h.iter().filter(|s| s.is(CCTL)).count()
        ),
        (RF_A, want.iter().collect::<Vec<_>>(), 1),
        "TX path A: CMAC table H2C, RF mode mux"
    );
    let (ops, _) = run(&mut l);
    assert!(
        !ops.iter()
            .any(|x| x.starts_with(&format!("rmw32 0x{:08X}", BB + R_P0_RFMODE))),
        "the same path again: no change"
    );
    l.run.phy.hal.tx_path_diversity = false;

    track::track_stop(&mut l.sta);
    let m = l.mark();
    l.env.now += 10 * TRACK_MS;
    let now = l.env.now;
    l.go(|b, c, s, _| track::track_due(b, c, s, now)).unwrap();
    assert_eq!(l.fake.bus.lines().len(), m.1, "stopped: no run");
    let _ = l.fake.finish();
}
