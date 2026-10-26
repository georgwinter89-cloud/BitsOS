//! js/test-sta.mjs section 2 ported: the H2C payloads against the values the JS test computed by
//! hand from the C masks (fw.h RTW89_H2C_*, cam.h ADDR_CAM_*), the PS-Poll template, the EDCA
//! value, DEFAULT_AX_RX_FLTR, rate adaptation for a legacy AP, our RSN element; then what the
//! fake AP runs cannot reach: the duplicate check on QoS data, DIG's restore, the CAM's limits,
//! a torn C2H. (tests/sta_c.rs checks the layouts and header bits against the C itself.)

use alloc::vec;
use alloc::vec::Vec;

use super::cam::{self, CamInfo};
use super::h2c::*;
use super::ra::{self, Ewma, conf_tx_edca_val, ra_assoc};
use super::*;
use crate::membus::MemBus;
use crate::phy::Phy;
use crate::tables::regs::*;

const OWN: [u8; 6] = [0x02, 0xb1, 0x75, 0x00, 0x00, 0x01];

/// test-sta.mjs's vif: MAC ID 0, port 0, station role, NORMAL key mode, a valid BSSID entry
fn vif() -> Vif {
    let mut v = Vif::new(OWN);
    v.wifi_role = RTW89_WIFI_ROLE_STATION;
    v.addr_cam = cam::AddrCam {
        idx: 0,
        offset: 0,
        len: 64,
        valid: true,
        addr_mask: 0,
        mask_sel: 0,
        sec_ent_mode: 2,
        sec_cam_map: 0,
        sec_ent_keyid: [0; 7],
        sec_ent: [0; 7],
        bssid_cam_idx: 0,
        wapi: 0,
    };
    v.bssid_cam = cam::BssidCam {
        idx: 0,
        phy_idx: 0,
        len: 8,
        offset: 0,
        valid: true,
        bssid: [0; 6],
    };
    v
}

fn dw(m: &H2cMsg) -> Vec<u32> {
    m.payload
        .chunks(4)
        .map(|c| {
            let mut b = [0u8; 4];
            b[..c.len()].copy_from_slice(c);
            u32::from_le_bytes(b)
        })
        .collect()
}

fn ap_link(mac_id: u32, supp_rates: u32, rssi_raw: u32) -> ApLink {
    let mut avg = Ewma::default();
    avg.add(rssi_raw);
    ApLink {
        mac_id,
        addr: [2, 0xaa, 0xbb, 0xcc, 0xdd, 6],
        avg_rssi: avg,
        rssi: [Ewma::default(); 2],
        supp_rates,
        rx_nss: 1,
        ra: None,
        er_cap: false,
        assoc_link: false,
        state: ApState::None,
        dup: wifi::replay::DupCheck::new(),
        rx_hw_rate: 0,
        prev_rssi: 0,
    }
}

#[test]
fn h2c_payloads_as_test_sta_computed_them() {
    let v = vif();
    assert_eq!(
        dw(&h2c_role_maintain(&v, None, RTW89_ROLE_CREATE)),
        [0x0000_2000],
        "wifi role 1 at bits 16:13"
    );
    assert_eq!(
        dw(&h2c_join_info(&v, None, true)),
        [0x0400_0100],
        "op 1, wifi role 1 at bits 29:26"
    );
    let mut infra = v.clone();
    infra.net_type = 2;
    assert_eq!(
        dw(&h2c_join_info(&infra, None, false)),
        [0x0600_0000],
        "INFRA at bits 25:24"
    );
    assert_eq!(
        dw(&h2c_macid_pause(0, 0, false)),
        [0, 0, 0, 0, 1, 0, 0, 0],
        "mask_grp[0] only"
    );
    assert_eq!(
        dw(&h2c_set_edca(&v, 3, 0xa432)),
        [0x60, 0xa432, 0],
        "AC at bits 6:5, param in dword 1"
    );
    assert_eq!(
        dw(&h2c_bcn_fltr_cfg(&v, true)),
        [0x0028_4c67],
        "monitor on, 60 beacons, hyst 4, -70 dBm"
    );
    let dc = dw(&h2c_default_cmac_tbl(&v, None, 0));
    assert_eq!(
        [
            dc[0], dc[6], dc[14], dc[5], dc[13], dc[1], dc[9], dc[7], dc[15]
        ],
        [
            0x80,
            0x43_0000,
            0xffff_0000,
            0,
            0xe00,
            0,
            0x20_0000,
            0,
            0x0f0c_0000
        ],
        "default CMAC table"
    );
    assert_eq!(
        dw(&h2c_default_cmac_tbl(&v, None, RF_B))[6],
        0x2_0000,
        "hal antenna_tx RF_B: path B only"
    );
    let ac = dw(&h2c_assoc_cmac_tbl(&v, Some(&ap_link(0, 0, 0)), true));
    assert_eq!(
        [
            ac[0], ac[1], ac[9], ac[2], ac[10], ac[11], ac[13], ac[6], ac[14], ac[7], ac[15],
            ac[16]
        ],
        [
            0x80,
            0x600_0000,
            0x600_0000,
            0,
            0xf000_0800,
            0x40,
            7,
            0,
            0x2000,
            0x2_0000,
            0xc0f2_0000,
            0x3000_0000
        ],
        "assoc CMAC table"
    );
    assert_eq!(
        h2c_assoc_cmac_tbl(&v, None, true).payload.len(),
        68,
        "H2C_CMC_TBL_LEN"
    );
    let c = dw(&h2c_addr_cam(&v, None, None));
    assert_eq!(c.len(), 15, "rtw89_h2c_addr_cam_v0: 15 dwords");
    assert_eq!(
        [c[1], c[2], c[4], c[5], c[9], c[12], c[13]],
        [
            0x40_0000,
            0x00c7_0001,
            0x0075_b102,
            0x0000_0100,
            0x2_0000,
            0x8_0000,
            0xfd
        ],
        "own MAC, NORMAL key mode, BSSID entry"
    );
    let mut p1 = v.clone();
    p1.port = 1;
    p1.mac_id = 5;
    p1.net_type = RTW89_NET_TYPE_INFRA;
    p1.aid = 0x123;
    let c1 = dw(&h2c_addr_cam(&p1, None, None));
    assert_eq!(
        [c1[8], c1[9] & 0xfff],
        [(1 << 8) | (1 << 11) | 5, 0x123],
        "PORT_INT, TSF_SYNC, MACID, AID12"
    );
    let gp = h2c_general_pkt(
        0,
        &GeneralPkts {
            ps_poll: Some(0),
            null_data: Some(1),
            qos_null: Some(2),
        },
    );
    assert_eq!(hex(&gp.payload), "00ff000102ff", "general packet ids");
    let ra = RaInfo {
        mode_ctrl: 3,
        bw_cap: 1,
        macid: 5,
        dcm_cap: 0,
        er_cap: 1,
        init_rate_lv: 2,
        upd_all: 1,
        en_sgi: 0,
        ldpc_cap: 1,
        stbc_cap: 0,
        ss_num: 1,
        giltf: 2,
        upd_bw_nss_mask: 0,
        upd_mask: 1,
        ra_mask: 0xf123_4567_89ab_cdef,
        fix_giltf_en: 1,
        fix_giltf: 3,
    };
    let m = h2c_ra(&ra);
    let w0 = (3 << 1)
        | (1 << 6)
        | (5 << 8)
        | (1 << 17)
        | (2 << 18)
        | (1 << 20)
        | (1 << 22)
        | (1 << 24)
        | (2 << 27)
        | 0x8000_0000u32;
    assert_eq!(
        dw(&m),
        [w0, 0x89ab_cdef, 0x7123_4567, (1 << 11) | (3 << 12)],
        "RA H2C v0, 16 bytes"
    );
    assert_eq!(
        (m.h.cat, m.h.cls, m.h.func),
        (
            H2C_CAT_OUTSRC,
            H2C_CL_OUTSRC_RA,
            H2C_FUNC_OUTSRC_RA_MACIDCFG
        )
    );
    let add = h2c_pkt_ofld_add(5, &[0xab; 16]);
    assert_eq!(
        (add.payload.len(), dw(&add)[0]),
        (20, 5 | (16 << 16)),
        "packet offload add"
    );
    assert!(add.payload[4..].iter().all(|&b| b == 0xab));
    assert_eq!(
        dw(&h2c_pkt_ofld_del(5)),
        [5 | (1 << 8)],
        "packet offload del: op 1, no length"
    );
    assert_eq!(
        dw(&h2c_rssi_offload(3, 0xa4)),
        [3 | (1 << 8), 0x52],
        "RSSI offload"
    );
    let d = PktDrop {
        sel: 3,
        macid: 5,
        band: 0,
        port: 1,
        mbssid: 0,
        tf_trs: 1,
    };
    assert_eq!(
        dw(&h2c_pkt_drop(&d)),
        [3 | (5 << 8) | (1 << 24), 1 << 8, 0, 0, 0, 0],
        "packet drop"
    );
    // fw.h RTW89_H2C_OFLD_W0_MAC_ID GENMASK(7, 0), _TX_TP GENMASK(17, 8), _RX_TP GENMASK(27, 18)
    assert_eq!(
        dw(&h2c_tp_offload(2, 76, 3))[0],
        2 | (76 << 8) | (3 << 18),
        "TP offload"
    );
}

fn hex(b: &[u8]) -> alloc::string::String {
    use core::fmt::Write;
    let mut s = alloc::string::String::new();
    for v in b {
        let _ = write!(s, "{v:02x}");
    }
    s
}

#[test]
fn templates_values_and_rate_adaptation() {
    assert_eq!(
        hex(&ops::ps_poll_frame(
            &[2, 0xaa, 0xbb, 0xcc, 0xdd, 6],
            &OWN,
            1
        )),
        "a40001c002aabbccdd0602b175000001",
        "PS-Poll template"
    );
    // non-QoS DCF, 2.4 GHz long slot: AIFS 2*20+10, ECWmin 4, ECWmax 10
    assert_eq!(
        conf_tx_edca_val(true, false, &wmm_default(false)[0]),
        0xa432
    );
    assert_eq!(
        conf_tx_edca_val(true, true, &wmm_default(false)[0]),
        0xa41c,
        "short slot: 2*9+10"
    );
    assert_eq!(
        conf_tx_edca_val(false, true, &wmm_default(false)[0]),
        0xa422,
        "5 GHz: 2*9+16"
    );
    assert_eq!(
        conf_tx_edca_val(true, false, &wmm_default(true)[0]),
        0xa532,
        "11b only: CWmin 31"
    );
    assert_eq!(
        crate::macinit::default_rx_fltr(),
        0x0300_44be,
        "DEFAULT_AX_RX_FLTR (reg.h:3348)"
    );
    // a legacy 2.4 GHz AP, strong signal (raw 120 = -50 dBm): the rate floor keeps 24..54 Mbit/s
    let ra = ra_assoc(&ap_link(0, 0xfff, 120), true, 2);
    assert_eq!(
        (ra.mode_ctrl, ra.ra_mask, ra.init_rate_lv, ra.ss_num),
        (3, 0xf00, 1, 0)
    );
    // weaker: lower floors; no RSSI: everything the AP has (level 0)
    assert_eq!(ra_assoc(&ap_link(0, 0xfff, 100), true, 2).ra_mask, 0xfe0);
    assert_eq!(
        ra_assoc(&ap_link(0, 0xfff, 0), true, 2).ra_mask,
        0xff5,
        "OFDM + sub-CCK"
    );
    assert_eq!(
        ra_assoc(&ap_link(0, 0x00f, 120), true, 2).mode_ctrl,
        1,
        "a b-only AP: CCK"
    );
    let five = ra_assoc(&ap_link(0, 0xff, 120), false, 2);
    assert_eq!(
        (five.mode_ctrl, five.ra_mask),
        (2, 0xf00),
        "5 GHz: the rates shifted past CCK"
    );
    assert_eq!(ra::ra_mask_recover(0, 0xff5), 0xff5);
    // the EWMA of the AP's RSSI (DECLARE_EWMA(rssi, 10, 16))
    let mut e = Ewma::default();
    e.add(100);
    e.add(116);
    assert_eq!(e.read(), 101);
    assert_eq!(
        hex(&wifi::rsn::own_ie(wifi::rsn::cipher::CCMP)),
        "30140100000fac040100000fac040100000fac020000",
        "our RSN element: CCMP, PSK, capabilities 0"
    );
}

#[test]
fn cam_bookkeeping_and_limits() {
    let mut info = CamInfo::default();
    let mut v = vif();
    v.addr_cam.valid = false;
    v.bssid_cam.valid = false;
    cam::cam_init(&mut info, &mut v).unwrap();
    assert_eq!(
        (
            v.addr_cam.idx,
            v.bssid_cam.idx,
            v.addr_cam.len,
            v.bssid_cam.len
        ),
        (0, 0, ADDR_CAM_ENT_SIZE, BSSID_CAM_ENT_SIZE)
    );
    // pairwise in slots 0-1, group in 2-4: a third group key is refused, nothing breaks
    let p = cam::new_sec_entry(&mut info, &[1; 16], 0, true, RTW89_SEC_KEY_TYPE_CCMP128).unwrap();
    assert_eq!(cam::attach_sec_cam(&mut v, &info, p).unwrap(), 0);
    let mut groups = Vec::new();
    for id in 1..=3 {
        let g =
            cam::new_sec_entry(&mut info, &[2; 16], id, false, RTW89_SEC_KEY_TYPE_CCMP128).unwrap();
        groups.push(g);
        assert_eq!(
            cam::attach_sec_cam(&mut v, &info, g).unwrap(),
            1 + id as usize
        );
    }
    let g4 = cam::new_sec_entry(&mut info, &[3; 16], 1, false, RTW89_SEC_KEY_TYPE_CCMP128).unwrap();
    assert!(
        cam::attach_sec_cam(&mut v, &info, g4).is_err(),
        "no free group slot"
    );
    assert_eq!(v.addr_cam.sec_cam_map, 0b11101);
    let cmd = cam::sec_key_cmd(info.sec_entry(p).unwrap());
    assert_eq!(
        (cmd.len(), &cmd[..8], &cmd[8..24]),
        (24, &[0, 0, 20, 0, 6, 0, 0, 0][..], &[1u8; 16][..])
    );
    cam::detach_sec_cam(&mut v, groups[0]);
    cam::free_sec_entry(&mut info, groups[0]);
    assert_eq!(v.addr_cam.sec_cam_map, 0b11001);
    assert!(info.sec_entry(groups[0]).is_none());
    assert!(
        cam::attach_sec_cam(&mut v, &info, groups[0]).is_err(),
        "a freed entry is not attached"
    );
    assert!(cam::new_sec_entry(&mut info, &[0; 33], 0, true, RTW89_SEC_KEY_TYPE_CCMP128).is_err());
    // the whole security CAM in use: refused, no panic
    let mut full = CamInfo::default();
    for _ in 0..cam::CamSizes::RTL8852B.sec {
        cam::new_sec_entry(&mut full, &[0; 16], 0, true, RTW89_SEC_KEY_TYPE_CCMP128).unwrap();
    }
    assert!(cam::new_sec_entry(&mut full, &[0; 16], 0, true, RTW89_SEC_KEY_TYPE_CCMP128).is_err());
    assert_eq!(full.sec_in_use(), cam::CamSizes::RTL8852B.sec);
    // the key never shows in Debug
    let shown = alloc::format!("{:?}", info.sec_entry(p));
    assert!(!shown.contains("1, 1, 1, 1"), "{shown}");
    cam::cam_deinit(&mut info, &mut v);
    assert!(!v.addr_cam.valid && !v.bssid_cam.valid);
}

#[test]
fn dig_restore_and_edcca_backup() {
    let mut bus = MemBus::new();
    let mut phy = Phy::new(1);
    phy.dig.bak_dig = 0x13;
    ra::dig_ctrl(&mut bus, &mut phy, 1, true, false).unwrap();
    ra::dig_ctrl(&mut bus, &mut phy, 1, true, false).unwrap(); // already paused: nothing
    ra::dig_ctrl(&mut bus, &mut phy, 1, false, true).unwrap();
    let pd = crate::phy::io::BB + R_SEG0R_PD_V1;
    let lines: Vec<alloc::string::String> = bus
        .lines()
        .into_iter()
        .filter(|l| l.contains(&alloc::format!("{pd:08X}")))
        .collect();
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert!(
        lines[2].ends_with(&alloc::format!(
            "0x{:08X}",
            crate::bus::prep(B_SEG0R_PD_LOWER_BOUND_MSK, 0x13)
        )),
        "restore: bak_dig"
    );
    assert!(
        lines[3].ends_with(&alloc::format!(
            "0x{:08X}",
            B_SEG0R_PD_SPATIAL_REUSE_EN_MSK_V1
        )),
        "linked: spatial reuse on"
    );
    let mut bak = ra::EdccaBak::default();
    let lvl = crate::phy::io::BB + R_SEG0R_EDCCA_LVL_V1;
    bus.poke(lvl, crate::bus::Width::W32, 0x4030_2010);
    ra::phy_config_edcca(&mut bus, &mut bak, true).unwrap();
    assert_eq!(
        bak,
        ra::EdccaBak {
            a: 0x10,
            p: 0x20,
            ppdu: 0x40
        },
        "EDCCA backup"
    );
    assert_eq!(
        bus.peek(lvl, crate::bus::Width::W32) & 0xff00_ffff,
        0xf900_f9f9
    );
    ra::phy_config_edcca(&mut bus, &mut bak, false).unwrap();
    assert_eq!(
        bus.peek(lvl, crate::bus::Width::W32),
        0x4030_2010,
        "restored"
    );
}

fn rx_frame(fc: u16, a1: [u8; 6], a2: [u8; 6], seq: u16, qos: Option<u16>) -> RxFrame {
    let mut f = vec![0u8; 24];
    f[0..2].copy_from_slice(&fc.to_le_bytes());
    f[4..10].copy_from_slice(&a1);
    f[10..16].copy_from_slice(&a2);
    f[16..22].copy_from_slice(&a2);
    f[22..24].copy_from_slice(&(seq << 4).to_le_bytes());
    if let Some(q) = qos {
        f.extend_from_slice(&q.to_le_bytes());
    }
    f.extend_from_slice(&[0xaa, 0xaa, 3, 0, 0, 0, 8, 0, 1, 2, 3, 4]);
    let hdr = crate::txrx::ieee::hdr_parse(&f).unwrap();
    f.extend_from_slice(&[0; 4]); // FCS
    RxFrame {
        entry: crate::txrx::RxEntry {
            desc: crate::txrx::RxDesc::default(),
            frame: f,
            status: crate::txrx::RxStatus::default(),
            ppdu: None,
            ppdu_desc: None,
            ppdu_match: false,
        },
        hdr,
    }
}

#[test]
fn duplicates_count_per_tid_for_qos_data() {
    let mut sta = Sta::new();
    let ap = ap_link(0, 0xfff, 0);
    let bssid = ap.addr;
    sta.ap = Some(ap);
    const RETRY: u16 = 0x0800;
    // QoS data (stype 8): TID 5 and TID 6 count apart; a retry with the same sequence control
    // of the same TID is a duplicate
    let q5 = rx_frame(0x0288, OWN, bssid, 7, Some(5));
    assert!(!link_dup(&mut sta, &q5));
    let q6 = rx_frame(0x0288 | RETRY, OWN, bssid, 7, Some(6));
    assert!(!link_dup(&mut sta, &q6), "another TID");
    let q5r = rx_frame(0x0288 | RETRY, OWN, bssid, 7, Some(5));
    assert!(link_dup(&mut sta, &q5r), "TID 5 again");
    // non-QoS data counts on the 17th counter, apart from the TIDs
    let d = rx_frame(0x0208 | RETRY, OWN, bssid, 7, None);
    assert!(!link_dup(&mut sta, &d));
    assert!(link_dup(&mut sta, &d));
    // another sender, group-addressed: never
    let other = rx_frame(0x0208 | RETRY, OWN, [9; 6], 7, None);
    assert!(!link_dup(&mut sta, &other) && !link_dup(&mut sta, &other));
    let bc = rx_frame(0x0208 | RETRY, [0xff; 6], bssid, 8, None);
    assert!(!link_dup(&mut sta, &bc) && !link_dup(&mut sta, &bc));
}

fn link_dup(sta: &mut Sta, f: &RxFrame) -> bool {
    link::duplicate(sta, f)
}

#[test]
fn torn_and_foreign_c2hs_change_nothing() {
    use crate::fw::C2h;
    let mut sta = Sta::new();
    sta.vif.mac_id = 0;
    let c = |cat, cls, func, payload: Vec<u8>| C2h {
        cat,
        cls,
        func,
        len: 8 + payload.len() as u32,
        seq: None,
        payload,
        ack: None,
    };
    // the decisions handle_c2h makes before any H2C: a torn report, another MAC ID, another type
    for (m, why) in [
        (
            c(
                RTW89_C2H_CAT_MAC,
                RTW89_MAC_C2H_CLASS_OFLD,
                RTW89_MAC_C2H_FUNC_BCNFLTR_RPT,
                vec![0, 1],
            ),
            "torn",
        ),
        (
            c(
                RTW89_C2H_CAT_MAC,
                RTW89_MAC_C2H_CLASS_OFLD,
                RTW89_MAC_C2H_FUNC_BCNFLTR_RPT,
                vec![3, RTW89_BCN_FLTR_BEACON_LOSS as u8, 0, 0],
            ),
            "MAC ID 3",
        ),
        (
            c(
                RTW89_C2H_CAT_MAC,
                RTW89_MAC_C2H_CLASS_OFLD,
                RTW89_MAC_C2H_FUNC_BCNFLTR_RPT,
                vec![0, 1, 0, 0],
            ),
            "RSSI, not a loss",
        ),
        (
            c(
                RTW89_C2H_CAT_MAC,
                RTW89_MAC_C2H_CLASS_OFLD,
                RTW89_MAC_C2H_FUNC_PKT_OFLD_RSP,
                vec![0, 0, 0, 0],
            ),
            "another function",
        ),
    ] {
        assert!(!ops::beacon_loss_report(&sta, &m), "{why}");
    }
    let ok = c(
        RTW89_C2H_CAT_MAC,
        RTW89_MAC_C2H_CLASS_OFLD,
        RTW89_MAC_C2H_FUNC_BCNFLTR_RPT,
        vec![0, RTW89_BCN_FLTR_BEACON_LOSS as u8, 0, 0],
    );
    assert!(ops::beacon_loss_report(&sta, &ok));
}

/// An environment whose clock never moves (a broken timer): waits must still end
struct Stuck;
impl StaEnv<MemBus> for Stuck {
    fn now_ms(&mut self, _: &mut MemBus) -> u64 {
        7
    }
    fn sleep_ms(&mut self, _: &mut MemBus, _: u32) {}
    fn random(&mut self, _: &mut [u8]) -> bool {
        false
    }
}

#[test]
fn a_clock_that_does_not_move_cannot_make_a_wait_endless() {
    let mut bus = MemBus::new();
    let mut env = Stuck;
    let mut c = WaitClock::new(&mut bus, &mut env);
    let end = c.now(&mut bus, &mut env) + 100;
    let mut n = 0;
    while c.now(&mut bus, &mut env) < end {
        c.sleep(&mut bus, &mut env, 1);
        n += 1;
        assert!(n <= 100);
    }
    assert_eq!(n, 100, "the sleeps count as time");
    let mut c0 = WaitClock::new(&mut bus, &mut env);
    c0.sleep(&mut bus, &mut env, 0);
    assert_eq!(
        c0.now(&mut bus, &mut env),
        8,
        "a zero sleep still counts 1 ms"
    );
    // the B6 names of the station's events
    assert_eq!(StaEvent::BeaconLoss.name(), "link.beacon_loss");
    assert_eq!(StaEvent::Wifi(wifi::WifiEvent::AuthOk).name(), "auth.ok");
}
