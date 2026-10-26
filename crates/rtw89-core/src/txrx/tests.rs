//! js/test-txrx.mjs ported check by check, in its order (no node). Expected descriptor words
//! are written by hand from the bit positions in txrx.h, the 802.11 frames byte by byte from
//! the 802.11-2020 layouts (as the JS test has them).

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use super::data::{CcmpKey, DataTx};
use super::ieee::*;
use super::parse::*;
use super::*;
use crate::pci::fill_txdesc;
use crate::phy::Band;

pub(super) fn hx(s: &str) -> Vec<u8> {
    let d: Vec<u8> = s
        .bytes()
        .filter(u8::is_ascii_hexdigit)
        .map(|c| (c as char).to_digit(16).unwrap() as u8)
        .collect();
    d.chunks(2).map(|p| p[0] << 4 | p[1]).collect()
}

pub(super) fn le32s(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn dws(b: &[u8]) -> Vec<u32> {
    b.chunks(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

pub(super) fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// the Mini-PC's MAC from its logical efuse (efuse-log.bin at 0x400; tests/txrx_js.rs reads it)
pub(super) const OWN: Mac = [0x02, 0xb1, 0x75, 0x00, 0x00, 0x01];
pub(super) const AP: Mac = [0x3c, 0xa6, 0x2f, 0x11, 0x22, 0x33];
const GW: Mac = [0x3c, 0xa6, 0x2f, 0x11, 0x22, 0x34];
const MCAST: Mac = [0x01, 0x00, 0x5e, 0x00, 0x00, 0x01];
pub(super) const SSID: &str = "FRITZ!Box 7590 XY";

fn h(m: &Mac) -> String {
    m.iter().map(|b| alloc::format!("{b:02x}")).collect()
}

/// test-txrx.mjs's FritzBox-like beacon (WPA2/WPA3 transition, PMF capable), FCS at the end
pub(super) fn beacon() -> Vec<u8> {
    let ap = h(&AP);
    let ssid: String = SSID.bytes().map(|b| alloc::format!("{b:02x}")).collect();
    hx(&alloc::format!(
        "80 00 0000 ffffffffffff {ap} {ap} 10a5
  0011223344550000 6400 3104
  00 11 {ssid}
  01 08 82 84 8b 96 0c 12 18 24   03 01 06   05 04 00 01 00 00   07 06 44 45 20 01 0d 14   2a 01 00
  32 04 30 48 60 6c
  30 1e 0100 000fac04 0100 000fac04 0200 000fac02 000fac08 8c00 0000 000fac06
  2d 1a ad01 17 ffff 0000000000000000000000000000 0000 00000000 00
  3d 16 06 0000000000 00000000000000000000000000000000
  dd 18 0050f2020101 80 00 03a40000 27a40000 42435e00 62322f00
  deadbeef"
    ))
}

pub(super) fn arp() -> Vec<u8> {
    cat(&[
        &hx("ffffffffffff"),
        &OWN,
        &hx("0806 0001 0800 06 04 0001"),
        &OWN,
        &hx("c0a86465 000000000000 c0a86401"),
    ])
}

pub(super) fn ip() -> Vec<u8> {
    hx("4500 001c 0000 4000 4001 0000 c0a86401 c0a86465 0800 f7ff 0000 0000")
}

/// a CCMP data frame From DS (CCMP header PN 5, MIC at the end, no FCS)
pub(super) fn from_ap() -> Vec<u8> {
    cat(&[
        &hx(&alloc::format!(
            "08 42 2c00 {} {} {} 5000  0500 0020 00000000  aaaa03000000 0800",
            h(&OWN),
            h(&AP),
            h(&GW)
        )),
        &ip(),
        &hx("1122334455667788"),
    ])
}

pub(super) fn chan(band: Band, ch: u32) -> Option<TxrxChan> {
    Some(TxrxChan {
        band,
        ch,
        prev: None,
    })
}

fn plain(eth: &[u8], key: Option<&mut CcmpKey>, seq: u16) -> Vec<u8> {
    eth_to_wlan(
        eth,
        DataTx {
            bssid: AP,
            key,
            qos: None,
            seq,
        },
    )
    .unwrap()
}

#[test]
fn frames_the_station_sends() {
    assert_eq!(mac_str(&OWN), "02:b1:75:00:00:01");
    assert_eq!(parse_mac("3c:a6:2f:11:22:33"), Some(AP));
    assert_eq!(parse_mac("3c-a6-2f-11-22-33"), Some(AP));
    assert_eq!(parse_mac("3c:a6:2f:11:22"), None);
    assert_eq!(parse_mac("3c:a6:2f:11:22:33:44"), None);
    assert_eq!(parse_mac("3c:a6:2f:11:22:zz"), None);
    let (own, ap) = (h(&OWN), h(&AP));
    let mut p = ProbeReq::new(OWN);
    p.ch = Some(6);
    assert_eq!(
        probe_req(&p),
        hx(&alloc::format!(
            "40 00 0000 ffffffffffff {own} ffffffffffff 0000  0000
  01 08 02 04 0b 16 0c 12 18 24  32 04 30 48 60 6c  03 01 06"
        )),
        "probe request 2.4 GHz, wildcard SSID"
    );
    let mut p = ProbeReq::new(OWN);
    p.band = Band::B5g;
    p.ssid = b"AB";
    assert_eq!(
        probe_req(&p),
        hx(&alloc::format!(
            "40 00 0000 ffffffffffff {own} ffffffffffff 0000
  00 02 41 42  01 08 0c 12 18 24 30 48 60 6c"
        )),
        "probe request 5 GHz, directed"
    );
    assert_eq!(
        auth_open(&OWN, &AP, 1),
        hx(&alloc::format!(
            "b0 00 0000 {ap} {own} {ap} 1000 0000 0100 0000"
        )),
        "open authentication"
    );
    let rsn = rsn_ie(&RsnIeReq::default());
    assert_eq!(
        rsn,
        hx("30 14 0100 000fac04 0100 000fac04 0100 000fac02 0000"),
        "RSN element WPA2-PSK CCMP"
    );
    let ap_rates = hx("82 84 8b 96 0c 12 18 24 30 48 60 6c");
    let a = AssocReq {
        own: OWN,
        bssid: AP,
        ssid: SSID.as_bytes(),
        band: Band::B2g,
        ap_rate_bytes: Some(&ap_rates),
        ap_capability: 0x0431,
        rsn: Some(&rsn),
        listen_interval: 5,
        seq: 2,
    };
    let ssid_hex: String = SSID.bytes().map(|b| alloc::format!("{b:02x}")).collect();
    assert_eq!(
        assoc_req(&a),
        hx(&alloc::format!(
            "00 00 0000 {ap} {own} {ap} 2000  3104 0500  00 11 {ssid_hex}
    01 08 02 04 0b 16 0c 12 18 24  32 04 30 48 60 6c  30 14 0100 000fac04 0100 000fac04 0100 000fac02 0000"
        )),
        "association request: ESS|privacy|short preamble|short slot, listen 5, rates, RSN"
    );
    let g = hx("8c 12 98 24 b0 48 60 6c");
    let a = AssocReq {
        ssid: b"x",
        ap_rate_bytes: Some(&g),
        ap_capability: 0,
        rsn: None,
        seq: 0,
        ..a
    };
    assert_eq!(
        assoc_req(&a)[24 + 4 + 3..].to_vec(),
        hx("01 08 0c 12 18 24 30 48 60 6c"),
        "association request to a g-only AP: only the common rates"
    );
    assert_eq!(
        deauth(&OWN, &AP, WLAN_REASON_DEAUTH_LEAVING, 3),
        hx(&alloc::format!("c0 00 0000 {ap} {own} {ap} 3000 0300")),
        "deauthentication, reason 3"
    );
    assert_eq!(
        null_func(&OWN, &AP, 4, None),
        hx(&alloc::format!("48 01 0000 {ap} {own} {ap} 4000")),
        "null data To DS, PM clear"
    );
    assert_eq!(
        null_func(&OWN, &AP, 0, Some(6)),
        hx(&alloc::format!("c8 01 0000 {ap} {own} {ap} 0000 0600")),
        "QoS null"
    );
    assert_eq!(
        [
            hdr_len(fc_of(T_MGMT, st::BEACON, fctl::ORDER)),
            hdr_len(fc_of(T_DATA, st::QOS_DATA, fctl::ORDER)),
            hdr_len(0xd4)
        ],
        [28, 30, 10],
        "header lengths with the Order bit (HT control), and of an ACK"
    );
    assert!(hdr_parse(&[]).is_none() && hdr_parse(&[0; 3]).is_none());
    assert_eq!(hdr_parse(&[0; 4]).unwrap().hdr_len, 24);
}

#[test]
fn data_frames_both_ways() {
    let mut ptk = CcmpKey::new(0, Some(3));
    let arp = arp();
    let w1 = plain(&arp, Some(&mut ptk), 0);
    let want = cat(&[
        &hx(&alloc::format!(
            "08 41 0000 {} {} ffffffffffff 0000  0100 0020 00000000  aaaa03000000 0806",
            h(&AP),
            h(&OWN)
        )),
        &arp[14..],
    ]);
    assert_eq!(
        w1, want,
        "ARP as 802.11: To DS|Protected, addr1 BSSID, CCMP header PN 1, RFC 1042"
    );
    assert_eq!(ptk.tx_pn, 1, "tx PN counts from 1");
    assert_eq!(
        ccmp_hdr(0x0102_0304_0506, 2).to_vec(),
        hx("06 05 00 a0 04 03 02 01"),
        "CCMP header PN byte order and key id"
    );
    let w = plain(&cat(&[&OWN, &OWN, &hx("8137 00")]), None, 0);
    assert_eq!(
        w[24..].to_vec(),
        hx("aaaa030000f8 8137 00"),
        "IPX uses the bridge-tunnel header"
    );
    let from_ap = from_ap();
    let e = wlan_to_eth(&from_ap).unwrap();
    assert_eq!(
        e.eth,
        cat(&[&OWN, &GW, &hx("0800"), &ip()]),
        "From DS frame -> Ethernet"
    );
    assert_eq!(
        (e.pn, e.key_id, e.sec_idx, e.eapol, mac_str(&e.bssid)),
        (
            Some(5),
            Some(0),
            0,
            false,
            String::from("3c:a6:2f:11:22:33")
        ),
        "RX CCMP fields"
    );
    let mut no_ext_iv = from_ap.clone();
    no_ext_iv[24 + 3] = 0;
    assert!(
        wlan_to_eth(&no_ext_iv).is_none(),
        "protected frame without ExtIV: not CCMP, dropped"
    );
    let qos = cat(&[
        &hx(&alloc::format!(
            "88 42 2c00 {} {} {} 5000 0900  0600 0020 00000000 aaaa03000000 0800",
            h(&OWN),
            h(&AP),
            h(&GW)
        )),
        &ip(),
        &hx("1122334455667788"),
    ]);
    let q = wlan_to_eth(&qos).unwrap();
    assert_eq!(
        (q.tid, q.sec_idx, q.pn, q.eth == e.eth),
        (9, 9, Some(6), true),
        "QoS data TID 9: replay counter 9 (TID mask 0xf)"
    );
    assert_eq!(
        [
            ccmp_replay(&mut ptk, 0, 5),
            ccmp_replay(&mut ptk, 0, 5),
            ccmp_replay(&mut ptk, 0, 6)
        ],
        [false, true, false],
        "PN replay check"
    );
    let mut gtk = CcmpKey::new(1, None);
    gtk.rsc = 10;
    assert_eq!(
        [ccmp_replay(&mut gtk, 0, 10), ccmp_replay(&mut gtk, 0, 11)],
        [true, false],
        "GTK replay counter starts at the RSC"
    );
    assert_eq!((ptk.replays, gtk.replays), (1, 1));
    let short = eth_to_wlan(
        &[0; 13],
        DataTx {
            bssid: AP,
            key: None,
            qos: None,
            seq: 0,
        },
    );
    assert!(short.is_err(), "an Ethernet frame without its header");
}

#[test]
fn beacon_and_responses_parse() {
    let beacon = beacon();
    let mpdu = &beacon[..beacon.len() - 4];
    let b = beacon_parse(mpdu).unwrap();
    assert_eq!(
        (
            b.beacon,
            b.ssid.as_str(),
            b.ch,
            b.beacon_int,
            b.privacy,
            mac_str(&b.bssid)
        ),
        (
            true,
            SSID,
            Some(6),
            100,
            true,
            String::from("3c:a6:2f:11:22:33")
        ),
        "beacon header and SSID"
    );
    assert_eq!(
        (b.rates_500k.clone(), b.basic_rates_500k.clone()),
        (
            vec![2, 4, 11, 22, 12, 18, 24, 36, 48, 72, 96, 108],
            vec![2, 4, 11, 22]
        ),
        "beacon rates"
    );
    let rsn = b.rsn.clone().unwrap();
    assert_eq!(
        (
            rsn.group,
            rsn.pairwise.clone(),
            rsn.akms.clone(),
            rsn.caps,
            rsn.group_mgmt
        ),
        (
            cipher::CCMP,
            vec![cipher::CCMP],
            vec![akm::PSK, akm::SAE],
            0x8c,
            Some(cipher::BIP_CMAC_128)
        ),
        "RSN element: CCMP, PSK + SAE, BIP"
    );
    assert_eq!(
        (rsn.mfpc, rsn.mfpr, rsn.psk, rsn.ccmp, b.wpa),
        (true, false, true, true, false),
        "RSN: PMF capable, not required"
    );
    let ht = b.ht.unwrap();
    assert_eq!(
        (ht.primary_ch, ht.cap_info, b.country, b.dtim_period),
        (Some(6), Some(0x01ad), Some(*b"DE"), Some(1)),
        "HT, country, DTIM"
    );
    assert_eq!(
        b.rsn_ie.as_deref().map(<[u8]>::len),
        Some(32),
        "the RSN element as sent"
    );
    assert_eq!(
        [
            rate_bitmap(&b.rate_bytes, Band::B2g, true),
            rate_bitmap(&b.rate_bytes, Band::B2g, false)
        ],
        [0x00f, 0xfff],
        "basic and supported rate bitmaps"
    );
    let mut ht_op = vec![11u8];
    ht_op.extend([0; 21]);
    let ies = parse_ies(&elem(eid::HT_OPERATION, &ht_op), 0, usize::MAX);
    assert_eq!(
        ies_channel(&ies),
        Some(11),
        "no DS element: channel from the HT operation"
    );
    assert_eq!(
        rsn_parse(&hx("0100 000fac04")).unwrap().pairwise,
        vec![cipher::CCMP],
        "RSN with omitted fields takes the defaults"
    );
    let resp = hx(&alloc::format!(
        "10 00 0000 {} {} {} 0000 3104 0000 01c0 010482848b96",
        h(&OWN),
        h(&AP),
        h(&AP)
    ));
    let m = mgmt_parse(&resp).unwrap();
    assert!(
        matches!(
            m.kind,
            MgmtKind::AssocResp {
                aid: 1,
                status: 0,
                ..
            }
        ),
        "association response AID"
    );
}

#[test]
fn queue_channel_and_wd_info() {
    let got: Vec<(u32, bool, u32)> = (0..8)
        .map(|t| (get_qsel(t), get_tid_indicate(t), get_ch_dma(get_qsel(t))))
        .collect();
    assert_eq!(
        got,
        [
            (0, false, 0),
            (1, false, 1),
            (1, true, 1),
            (0, true, 0),
            (2, false, 2),
            (2, true, 2),
            (3, false, 3),
            (3, true, 3)
        ],
        "TID 0-7: QSEL BE BK BK BE VI VI VO VO, TID_INDICATE for 2 3 5 7, DMA channel ACH0-3"
    );
    let ch: Vec<u32> = [0x11, 0x12, 0x19, 0x1a, 0x05, 0x0e]
        .iter()
        .map(|&q| get_ch_dma(q))
        .collect();
    assert_eq!(
        ch,
        [9, 8, 11, 10, 1, 2],
        "B0_HI CH9, B0_MGMT CH8, B1_HI CH11, B1_MGMT CH10, BK_1, VI_3"
    );
    let i = TxInfo {
        use_rate: true,
        data_bw: 2,
        gi_ltf: 5,
        data_rate: 0x1ab,
        stbc: true,
        ldpc: true,
        dis_data_fb: true,
        port: 3,
        tx_cnt_lmt_en: true,
        tx_cnt_lmt: 8,
        data_retry_lowest_rate: 0x0b,
        a_ctrl_bsr: true,
        ampdu_num: 0x3f,
        ampdu_density: 5,
        sec_type: 6,
        sec_en: true,
        sec_cam_idx: 0x42,
        report: true,
        is_bmc: false,
        sn: 9,
        ..TxInfo::default()
    };
    assert_eq!(
        wd_info_dwords(&i),
        [0x6bab1c30, 0x900b403f, 0x00140d42, 0x400, 0x88000009, 0],
        "WD info: every field at its C bit position"
    );
}

#[test]
fn tx_info_management() {
    let mut d = Txrx::new();
    d.chan = chan(Band::B2g, 6);
    let auth = auth_open(&OWN, &AP, 0x123);
    let o = TxOpts {
        mac_id: Some(0),
        sta: true,
        basic_rates: 0x00f,
        ..TxOpts::default()
    };
    let info = tx_info(&mut d, Some(TxKind::Mgmt), &auth, &o).unwrap();
    assert_eq!(
        (
            info.qsel,
            info.ch_dma,
            info.hw_ssn_sel,
            info.hw_seq_mode,
            info.data_rate,
            info.is_bmc
        ),
        (0x12, 8, 1, 1, 0, false),
        "auth: B0_MGMT on CH8, HW SSN, CCK1"
    );
    assert_eq!(
        dws(&fill_txdesc(&info.desc())),
        [
            0x00480085, 0, 0x0024001e, 0x123, 0, 0, 0x40000400, 0, 0, 0, 0x88000000, 0
        ],
        "auth WD body + info"
    );
    let mut p = ProbeReq::new(OWN);
    p.ch = Some(6);
    let info = tx_info(&mut d, None, &probe_req(&p), &TxOpts::default()).unwrap();
    let wd = info.wd_info.unwrap();
    assert_eq!(
        (wd[0], wd[4]),
        (0x40000400, 0x80000000),
        "broadcast probe: CCK1, no RTS"
    );
    let mut p = ProbeReq::new(OWN);
    p.dst = MCAST;
    let info = tx_info(&mut d, None, &probe_req(&p), &TxOpts::default()).unwrap();
    assert_eq!(
        info.wd_info.unwrap()[4],
        0x80000000,
        "multicast addr1: no RTS either (is_bmc)"
    );
    let g = TxOpts {
        sta: true,
        basic_rates: 0xff0,
        ..TxOpts::default()
    };
    let info = tx_info(&mut d, None, &auth, &g).unwrap();
    assert_eq!(
        info.wd_info.unwrap()[0],
        0x40040400,
        "g-only AP: management rate OFDM6 (DATA_RATE 4 << 16)"
    );
    let no_sta = TxOpts {
        sta: false,
        ..g.clone()
    };
    assert_eq!(
        tx_info(&mut d, None, &auth, &no_sta).unwrap().data_rate,
        0,
        "no station entry yet: lowest rate, not the basic rates"
    );
    let five = TxOpts {
        band: Some(Band::B5g),
        no_cck: true,
        ..TxOpts::default()
    };
    assert_eq!(
        tx_info(&mut d, None, &auth, &five).unwrap().data_rate,
        4,
        "5 GHz management rate OFDM6"
    );
    let null = null_func(&OWN, &AP, 0, None);
    let info = tx_info(&mut d, None, &null, &TxOpts::default()).unwrap();
    assert_eq!(
        info.tx_type,
        Some(TxType::Mgmt),
        "null data goes as management (any_nullfunc)"
    );
}

#[test]
fn tx_info_data() {
    let mut d = Txrx::new();
    d.chan = chan(Band::B2g, 6);
    let mut ptk = CcmpKey::new(0, Some(3));
    let w2 = plain(&arp(), Some(&mut ptk), 7);
    let key = Some(TxKey::from(&ptk));
    let o = TxOpts {
        mac_id: Some(0),
        sta: true,
        supp_rates: 0xff0,
        key,
        ..TxOpts::default()
    };
    let info = tx_info(&mut d, Some(TxKind::Data), &w2, &o).unwrap();
    assert_eq!(
        (
            info.qsel,
            info.ch_dma,
            info.tid_indicate,
            info.hdr_llc_len,
            info.btc_pkt,
            info.bk
        ),
        (0, 0, false, 12, Some(BtcPkt::Arp), false),
        "ARP data: BE_0 on ACH0"
    );
    assert_eq!(
        dws(&fill_txdesc(&info.desc())),
        [
            0x00406080, 0, 0x44, 7, 0, 0, 0, 0x00040000, 0x00000d03, 0, 0x88000000, 0
        ],
        "CCMP data WD body + info"
    );
    let o2 = TxOpts {
        sta: false,
        supp_rates: 0xff0,
        key,
        ..TxOpts::default()
    };
    let info = tx_info(&mut d, Some(TxKind::Data), &w2, &o2).unwrap();
    assert_eq!(
        info.data_retry_lowest_rate, 0,
        "data: no station entry, lowest rate"
    );
    let eapol = plain(&cat(&[&AP, &OWN, &hx("888e 0203005f02")]), None, 8);
    let info = tx_info(&mut d, Some(TxKind::Data), &eapol, &TxOpts::default()).unwrap();
    assert_eq!(
        (
            info.btc_pkt,
            dws(&fill_txdesc(&info.desc()))[3],
            info.wd_info.unwrap()[2]
        ),
        (Some(BtcPkt::Eapol), 0x2008, 0),
        "EAPOL: BK bit (b13), no security"
    );
    d.tx.last_actrl = true; // as if the last data frame had carried an A-control BSR
    let ko = TxOpts {
        key,
        ..TxOpts::default()
    };
    let a = tx_info(&mut d, Some(TxKind::Data), &w2, &ko).unwrap().bk;
    let b = tx_info(&mut d, Some(TxKind::Data), &w2, &ko).unwrap().bk;
    assert_eq!(
        [a, b],
        [true, false],
        "A-control change: BK once (tail of rtw89_core_tx_update_he_qos_htc)"
    );
    let dhcp_to = |pad: usize| {
        let eth = cat(&[
            &hx("ffffffffffff"),
            &OWN,
            &hx("0800 4500 0148 0000 0000 4011 0000 00000000 ffffffff 0044 0043"),
            &vec![0u8; pad],
        ]);
        plain(&eth, None, 0)
    };
    let none = TxOpts::default();
    let big = tx_info(&mut d, Some(TxKind::Data), &dhcp_to(300), &none).unwrap();
    let small = tx_info(&mut d, Some(TxKind::Data), &dhcp_to(100), &none).unwrap();
    assert_eq!(
        [big.btc_pkt, small.btc_pkt],
        [Some(BtcPkt::Dhcp), None],
        "DHCP recognised for coex, only above 282 bytes"
    );
    let icmp = plain(&cat(&[&AP, &OWN, &hx("0800"), &ip()]), None, 0);
    let info = tx_info(&mut d, Some(TxKind::Data), &icmp, &none).unwrap();
    assert_eq!(info.btc_pkt, Some(BtcPkt::Icmp), "ICMP recognised for coex");
    assert!(
        tx_info(&mut d, Some(TxKind::Mgmt), &w2, &none).is_err(),
        "kind mismatch is refused"
    );
    assert!(
        tx_info(&mut d, Some(TxKind::Data), &w2, &none).is_err(),
        "protected frame without key is refused"
    );
    let fw = tx_info(&mut d, Some(TxKind::Fwcmd), &[0; 10], &none).unwrap();
    assert_eq!(fw.ch_dma, 12, "fwcmd info: CH12");
}

#[test]
fn sequence_numbers() {
    let mut d = Txrx::new();
    let mut s1 = deauth(&OWN, &AP, 3, 0);
    let mut s2 = deauth(&OWN, &AP, 3, 0);
    tx_seq(&mut d, &mut s1).unwrap();
    tx_seq(&mut d, &mut s2).unwrap();
    assert_eq!(
        [le16(&s1, 22), le16(&s2, 22)],
        [0x0000, 0x0010],
        "sequence control counts in 0x10 steps"
    );
    let qos_data = |tid: u8, a1: &Mac| {
        let mut f = hdr(fc_of(T_DATA, st::QOS_DATA, fctl::TODS), a1, &OWN, &AP, 0);
        f.extend([tid, 0]);
        f
    };
    let mut seq_of = |mut f: Vec<u8>| {
        tx_seq(&mut d, &mut f).unwrap();
        le16(&f, 22)
    };
    let got = [
        seq_of(qos_data(3, &AP)),
        seq_of(qos_data(3, &AP)),
        seq_of(qos_data(5, &AP)),
        seq_of(qos_data(6, &MCAST)),
    ];
    assert_eq!(
        (got, d.tx.seq_ctrl),
        ([0, 0x10, 0, 0x20], 0x30),
        "QoS data: a counter per TID; multicast QoS data: the interface counter"
    );
}

#[test]
fn rx_descriptor_fields() {
    let rxd = le32s(&[
        0x8000_0000 | (3 << 28) | (1 << 24) | (1 << 23) | (2 << 14) | 0x123,
        (2 << 30) | (5 << 25) | (0x085 << 16) | (0x12 << 8) | (1 << 7) | (6 << 4) | 9,
        0x1234_5678,
        1 | (1 << 2) | (1 << 3) | (1 << 9) | (1 << 10),
        2,
        5 | (7 << 8) | (9 << 16) | (10 << 24) | (1 << 28),
        0,
        6 << 17,
    ]);
    let buf = cat(&[&[0; 4], &rxd]);
    let mut q = RxDesc::default();
    query_rxdesc(&buf, 4, &mut q);
    assert_eq!(
        (
            q.pkt_size,
            q.drv_info_size,
            q.long_rxdesc,
            q.pkt_type,
            q.mac_info_valid,
            q.bw,
            q.data_rate,
            q.gi_ltf,
            q.user_id,
            q.sr_en,
            q.ppdu_cnt,
            q.ppdu_type
        ),
        (0x123, 3, true, 1, true, 2, 0x85, 5, 0x12, true, 6, 9),
        "RX desc dword 0/1"
    );
    assert_eq!(
        (
            q.free_run_cnt,
            q.addr1_match,
            q.sw_dec,
            q.hw_dec,
            q.ampdu,
            q.crc32_err,
            q.icv_err
        ),
        (0x12345678, true, false, true, true, true, true),
        "RX desc dword 2/3"
    );
    assert_eq!(
        (
            q.frame_type,
            q.sec_cam_id,
            q.addr_cam_id,
            q.mac_id,
            q.rx_pl_id,
            q.addr_cam_valid,
            q.sec_type
        ),
        (2, 5, 7, 9, 10, true, 6),
        "RX desc dword 4-7"
    );
    assert_eq!(
        (q.offset, q.rxd_len, q.shift, q.shift_field),
        (4 + 24, 32, 0, 2),
        "offset = data_offset + drv info (Linux ignores shift on AX)"
    );
}

// ---- RX buffers built by hand (test-txrx.mjs `item`, `rxdesc`, `ppduItem`) ----

pub(super) struct Item {
    pub data: Vec<u8>,
    pub fs: bool,
    pub ls: bool,
    pub len: u32,
}

impl Item {
    pub fn whole(body: Vec<u8>) -> Item {
        let len = body.len() as u32 + 4;
        Item {
            data: body,
            fs: true,
            ls: true,
            len,
        }
    }
    pub fn seg(d: &[u8], fs: bool, ls: bool) -> Item {
        Item {
            data: d.to_vec(),
            fs,
            ls,
            len: d.len() as u32 + 4,
        }
    }
    pub fn rx(&self) -> RxIn<'_> {
        RxIn {
            data: &self.data,
            fs: self.fs,
            ls: self.ls,
            len: Some(self.len),
        }
    }
}

#[derive(Default, Clone, Copy)]
pub(super) struct Rd {
    pub long: bool,
    pub typ: u32,
    pub mac_info: bool,
    pub len: u32,
    pub cnt: u32,
    pub rate: u32,
    pub gi: u32,
    pub dw3: u32,
}

pub(super) fn rxdesc(r: Rd) -> Vec<u8> {
    let mut w = vec![
        if r.long { 0x8000_0000 } else { 0 }
            | (r.typ << 24)
            | if r.mac_info { 1 << 23 } else { 0 }
            | r.len,
        (r.gi << 25) | (r.rate << 16) | (r.cnt << 4),
        0,
        r.dw3,
    ];
    if r.long {
        w.extend([0, 0, 0, 0]);
    }
    le32s(&w)
}

pub(super) fn bcn_item(cnt: u32) -> Item {
    let b = beacon();
    let d = rxdesc(Rd {
        long: true,
        len: b.len() as u32,
        cnt,
        ..Rd::default()
    });
    Item::whole(cat(&[&d, &b]))
}

#[derive(Default, Clone, Copy)]
pub(super) struct Pp {
    pub cnt: u32,
    pub rate: u32,
    pub gi: u32,
    pub not_a1: bool,
    pub ie01: bool,
    pub len_units: Option<u32>,
    pub rx_cnt: bool,
}

// PPDU status: mac info (1 user: MAC_ID_VALID | BCN; padding to 8 bytes; RX_CNT_VLD b29: 0x60
// bytes of counters), PHY status header (IE_MAP 11 | VALID b7 | LEN << 8 | RSSI_AVG << 24;
// RSSI_A | RSSI_B << 8), IE00 (RPL 0x98 << 7) and with ie01 an IE01 (type 1 | RSSI_AVG_FD 0x50
// << 8 | CH_IDX 6 << 16)
pub(super) fn ppdu_item(o: Pp) -> Item {
    let mut ies = le32s(&[0x98 << 7, 0, 0, 0]);
    if o.ie01 {
        ies.extend(le32s(&[1 | (0x50 << 8) | (6 << 16), 0, 0, 0, 0, 0, 0, 0]));
    }
    let n = o
        .len_units
        .unwrap_or((8 + 16 + if o.ie01 { 32 } else { 0 }) / 8);
    let p = cat(&[
        &le32s(&[if o.rx_cnt { 1 << 29 } else { 0 } | 1, 0, 0x11, 0]),
        &vec![0u8; if o.rx_cnt { 0x60 } else { 0 }],
        &le32s(&[11 | 0x80 | (n << 8) | (0x4c << 24), 0x4c | (0x48 << 8)]),
        &ies,
    ]);
    let d = rxdesc(Rd {
        typ: 1,
        mac_info: true,
        len: p.len() as u32,
        cnt: o.cnt,
        rate: o.rate,
        gi: o.gi,
        dw3: if o.not_a1 { 0 } else { 1 },
        ..Rd::default()
    });
    Item::whole(cat(&[&d, &p]))
}

/// stand-ins of phy's 8852B chip ops (the formulas of rtw8852b_common.c:1967-1996), recording
/// the rx_stats hook's (ppdu given, desc's packet type)
#[derive(Default)]
pub(super) struct Ops {
    pub stats: Vec<(bool, Option<u32>)>,
}

impl RxOps for Ops {
    fn convert_rpl_to_rssi(&mut self, p: &mut PhyPpdu) {
        let delta = p.rpl_avg.wrapping_sub(p.rssi_avg);
        p.rssi[0] = p.rssi[0].wrapping_add(delta);
        p.rssi[1] = p.rssi[1].wrapping_add(delta);
        p.rssi_avg = p.rpl_avg;
    }
    fn query_ppdu(&mut self, p: &PhyPpdu, st: &mut RxStatus) {
        st.signal = (i32::from(p.rssi[0].max(p.rssi[1])) >> 1) - 110;
        st.chains = 3;
    }
    fn rx_stats(&mut self, _e: &RxEntry, p: Option<&PhyPpdu>, d: Option<&RxDesc>) {
        self.stats.push((p.is_some(), d.map(|d| d.pkt_type)));
    }
}

fn fcs_data() -> Vec<u8> {
    cat(&[&from_ap(), &hx("01020304")])
}

#[test]
fn rx_beacon_waits_for_its_ppdu_status() {
    let mut ops = Ops::default();
    let mut rd = Txrx::new();
    rd.chan = chan(Band::B2g, 1);
    rd.scanning = true;
    let beacon = beacon();
    let head = le32s(&[
        0x8000_0000 | beacon.len() as u32,
        2 << 4,
        0x1000,
        0,
        0,
        0,
        0,
        0,
    ]);
    let rx_beacon = Item::whole(cat(&[&head, &beacon]));
    let out = rx_parse(&mut rd, rx_beacon.rx(), &mut ops);
    assert_eq!(
        (out.typ, out.pending, out.frame.is_none(), out.deliver.len()),
        (RxType::Frame, Some(beacon.len()), true, 0),
        "long-desc management frame waits for PPDU status (pending, not yet a frame)"
    );
    let pp = ppdu_item(Pp {
        cnt: 2,
        ..Pp::default()
    });
    let out = rx_parse(&mut rd, pp.rx(), &mut ops);
    let p = out.ppdu.clone().unwrap();
    assert_eq!(
        (
            out.typ,
            out.frame.is_none(),
            p.valid,
            p.rpl_avg,
            [p.rssi[0], p.rssi[1]],
            out.deliver.len()
        ),
        (RxType::Ppdu, true, true, 0x4c, [0x4c, 0x48], 1),
        "PPDU status parsed (mac info, header, IE00) and paired"
    );
    let ent = &out.deliver[0];
    assert_eq!(
        (
            ent.ppdu.as_ref() == Some(&p),
            ent.ppdu_desc.as_ref() == out.desc.as_ref(),
            ent.ppdu_match,
            ent.status.signal,
            ent.status.no_signal,
            ent.status.ch,
            ent.status.freq,
            ent.status.mactime
        ),
        (true, true, true, -72, false, Some(6), Some(2437), 0x1000),
        "beacon: RSSI from the PPDU, channel from its DS element while scanning"
    );
    assert_eq!(
        core::mem::take(&mut ops.stats),
        [(true, Some(1))],
        "rx_stats hook: the PPDU and the PPDU status desc, as rtw89_core_rx_pending_skb passes them"
    );
    assert_eq!(
        ent.mpdu(),
        &beacon[..beacon.len() - 4],
        "delivered frame without FCS"
    );
    assert_eq!(
        beacon_parse(ent.mpdu()).unwrap().ssid,
        SSID,
        "and it parses"
    );

    // C2H, drv info, PPDU counter flush, FCS error, segments
    let c2h = le32s(&[1 | (2 << 2) | (3 << 8), 12, 0xabcd]);
    let item = Item::whole(cat(&[
        &le32s(&[c2h.len() as u32 | (10 << 24), 0, 0, 0]),
        &c2h,
    ]));
    let out = rx_parse(&mut rd, item.rx(), &mut ());
    let c = out.c2h.clone().unwrap();
    assert_eq!(
        (
            out.typ,
            out.frame.as_deref() == Some(&c2h[..]),
            out.ppdu.is_none(),
            c.cat,
            c.cls,
            c.func,
            c.len
        ),
        (RxType::C2h, true, true, 1, 2, 3, 12),
        "C2H: the packet as frame (contract shape), handed to c2h_parse"
    );
    rx_parse(&mut rd, rx_beacon.rx(), &mut ops); // queued again, PPDU 2
    let fcs = fcs_data();
    let rx_data = Item::whole(cat(&[
        &le32s(&[fcs.len() as u32 | (1 << 28), 3 << 4, 0, 1 | 4]),
        &[0xee; 8],
        &fcs,
    ]));
    let out = rx_parse(&mut rd, rx_data.rx(), &mut ops);
    let d: Vec<(bool, usize)> = out
        .deliver
        .iter()
        .map(|x| (x.ppdu.is_some(), x.frame.len()))
        .collect();
    assert_eq!(
        (out.frame.as_deref() == Some(&fcs[..]), d),
        (true, vec![(false, beacon.len()), (false, fcs.len())]),
        "new PPDU counter flushes the queued beacon first; the data frame goes up at once"
    );
    assert_eq!(
        core::mem::take(&mut ops.stats),
        [(false, Some(0)), (false, Some(0))],
        "rx_stats hook without PPDU on the flush and the direct path"
    );
    let ent = &out.deliver[1];
    let same = wlan_to_eth(ent.mpdu()).unwrap().eth == wlan_to_eth(&from_ap()).unwrap().eth;
    assert_eq!(
        (ent.status.decrypted, same),
        (true, true),
        "short-desc data frame after 8 bytes drv info, decrypted"
    );
    let bad = Item::whole(cat(&[&le32s(&[fcs.len() as u32, 4 << 4, 0, 1 << 9]), &fcs]));
    let bufs = [crate::pci::RxBuf {
        data: bad.data.clone(),
        fs: true,
        ls: true,
        tag: 0,
        len: bad.len,
    }];
    assert_eq!(
        rd.rx_frames(&bufs, &mut ()).frames.len(),
        0,
        "CRC32 error: mac80211 drops it"
    );
    let whole = cat(&[&le32s(&[fcs.len() as u32, 5 << 4, 0, 4]), &fcs]);
    let (p1, p2) = (
        Item::seg(&whole[..40], true, false),
        Item::seg(&whole[40..], false, true),
    );
    assert_eq!(
        rx_parse(&mut rd, p1.rx(), &mut ()).typ,
        RxType::Partial,
        "first segment kept"
    );
    let out = rx_parse(&mut rd, p2.rx(), &mut ());
    assert_eq!(out.deliver[0].frame, fcs, "segments joined");
    assert_eq!(
        rx_parse(&mut rd, p2.rx(), &mut ()).typ,
        RxType::Dropped,
        "continuation without start dropped"
    );
    let cut = Item::whole(le32s(&[0x8000_0000 | 40, 0, 0, 0]));
    assert_eq!(
        rx_parse(&mut rd, cut.rx(), &mut ()).typ,
        RxType::Dropped,
        "long descriptor cut short: dropped, no exception"
    );
    let mut sg = Txrx::new();
    let long_seg = cat(&[
        &rxdesc(Rd {
            len: 10,
            ..Rd::default()
        }),
        &[0; 40],
    ]);
    assert_eq!(
        rx_parse(&mut sg, Item::seg(&long_seg, true, false).rx(), &mut ()).reason,
        Some("invalid length"),
        "first segment longer than RPKT_LEN: dropped"
    );
    sg.rx.desc.ready = true; // a descriptor left behind without its skb
    assert_eq!(
        rx_parse(&mut sg, Item::whole(whole.clone()).rx(), &mut ()).reason,
        Some("desc ready before first segment"),
        "descriptor ready before a first segment: dropped"
    );
    let mut rs = Txrx::new();
    rx_parse(&mut rs, p1.rx(), &mut ()); // half a frame, then a reset
    rs.pci_reset();
    assert_eq!(
        rx_parse(&mut rs, Item::whole(whole).rx(), &mut ()).deliver[0].frame,
        fcs,
        "pci_reset forgets the half frame (rtw89_pci_reset_trx_rings)"
    );
    assert!(
        rd.events().contains(&RxEvent::Dropped {
            reason: "no last skb"
        }),
        "drops are events (B6)"
    );
}

#[test]
fn rx_ppdu_pairing() {
    let mut ops = Ops::default();
    let mut pd = Txrx::new();
    pd.chan = chan(Band::B2g, 1);
    rx_parse(&mut pd, bcn_item(3).rx(), &mut ops);
    let pp = ppdu_item(Pp {
        cnt: 3,
        rate: 4,
        ..Pp::default()
    });
    let o = rx_parse(&mut pd, pp.rx(), &mut ops);
    let ent = &o.deliver[0];
    assert_eq!(
        (
            ent.ppdu.as_ref().map(|p| p.valid),
            ent.ppdu_match,
            ent.status.signal,
            ent.status.no_signal
        ),
        (Some(true), false, 0, true),
        "other rate: up with the PPDU, no RSSI"
    );
    rx_parse(&mut pd, bcn_item(4).rx(), &mut ops);
    let pp = ppdu_item(Pp {
        cnt: 4,
        gi: 1,
        ..Pp::default()
    });
    let o = rx_parse(&mut pd, pp.rx(), &mut ops);
    assert_eq!(
        (o.deliver[0].ppdu_match, o.deliver[0].status.signal),
        (false, 0),
        "other GI: no match"
    );
    rx_parse(&mut pd, bcn_item(5).rx(), &mut ops);
    let pp = ppdu_item(Pp {
        cnt: 5,
        ..Pp::default()
    });
    let o = rx_parse(&mut pd, pp.rx(), &mut ops);
    let ent = &o.deliver[0];
    assert_eq!(
        (ent.ppdu_match, ent.status.signal, ent.status.ch),
        (true, -72, Some(1)),
        "same counter, rate, GI, bandwidth: match; not scanning: channel kept"
    );

    // PPDU status parsing: IE gating, length check, RX counters
    let ppd = |o: Pp| {
        rx_parse(&mut Txrx::new(), ppdu_item(o).rx(), &mut Ops::default())
            .ppdu
            .unwrap()
    };
    let p = ppd(Pp {
        ie01: true,
        ..Pp::default()
    });
    assert_eq!(
        (p.valid, p.rpl_avg, p.chan_idx, p.ofdm.has),
        (true, 0x4c, 6, false),
        "CCK rate: IE01 gives the channel, not its RSSI"
    );
    let p = ppd(Pp {
        ie01: true,
        rate: 4,
        ..Pp::default()
    });
    assert_eq!(
        (p.valid, p.rpl_avg, p.chan_idx, p.ofdm.has),
        (true, 0x50, 6, true),
        "OFDM rate: RSSI from IE01 (RSSI_AVG_FD)"
    );
    let p = ppd(Pp {
        ie01: true,
        rate: 4,
        not_a1: true,
        ..Pp::default()
    });
    assert_eq!(
        (p.valid, p.rpl_avg, p.chan_idx),
        (true, 0, 6),
        "not to self: IE00 skipped, IE01 only for the channel"
    );
    let p = ppd(Pp {
        len_units: Some(4),
        ..Pp::default()
    });
    assert_eq!(
        (p.valid, p.rpl_avg, p.rssi),
        (false, 0, [0, 0, 0, 0]),
        "PHY status length other than its header says: invalid"
    );
    let p = ppd(Pp {
        rx_cnt: true,
        ..Pp::default()
    });
    assert_eq!(
        (p.valid, p.rpl_avg, p.start),
        (true, 0x4c, 8 + 4 + 4 + 0x60),
        "RX counters (RX_CNT_VLD) skipped before the PHY status"
    );
}

#[test]
fn rx_status_fields() {
    let fcs = fcs_data();
    let flags = |dw3: u32| {
        let d = rxdesc(Rd {
            len: fcs.len() as u32,
            dw3,
            ..Rd::default()
        });
        let it = Item::whole(cat(&[&d, &fcs]));
        let st = rx_parse(&mut Txrx::new(), it.rx(), &mut ())
            .deliver
            .remove(0)
            .status;
        (st.decrypted, st.failed_fcs)
    };
    assert_eq!(
        [flags(4), flags(4 | 2), flags(4 | 0x400), flags(0x200)],
        [(true, false), (false, false), (false, true), (false, true)],
        "HW_DEC: decrypted unless SW_DEC or ICV_ERR; ICV or CRC32 error: failed FCS"
    );
    let mut g5 = Txrx::new();
    g5.chan = Some(TxrxChan {
        band: Band::B5g,
        ch: 36,
        prev: Some((Band::B2g, 6)),
    });
    let d = rxdesc(Rd {
        len: fcs.len() as u32,
        rate: 4,
        ..Rd::default()
    });
    let it = Item::whole(cat(&[&d, &fcs]));
    let st = rx_parse(&mut g5, it.rx(), &mut ()).deliver.remove(0).status;
    assert_eq!(
        (st.band, st.ch, st.freq, st.rate_idx),
        (Some(Band::B5g), Some(36), Some(5180), 0),
        "5 GHz OFDM6: sband rate index 0 (rtw89_core_hw_to_sband_rate)"
    );
    rx_parse(&mut g5, bcn_item(2).rx(), &mut ()); // CCK beacon on the 5 GHz channel
    let pp = ppdu_item(Pp {
        cnt: 2,
        ..Pp::default()
    });
    let st = rx_parse(&mut g5, pp.rx(), &mut ()).deliver.remove(0).status;
    assert_eq!(
        (st.band, st.ch, st.freq, st.rate_idx),
        (Some(Band::B2g), Some(6), Some(2437), 0),
        "CCK frame on 5 GHz: the previous channel (rtw89_correct_cck_chan)"
    );
}

/// test-txrx.mjs's LCG
pub(super) struct Lcg(pub u32);

impl Lcg {
    pub fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(1_103_515_245).wrapping_add(12345);
        self.0 >> 24
    }
}

#[test]
fn random_buffers_never_panic() {
    let mut rnd = Txrx::new();
    rnd.chan = chan(Band::B5g, 36);
    rnd.scanning = true;
    let mut r = Lcg(1);
    let mut ops = Ops::default();
    for i in 0..3000u32 {
        let n = (r.next() % 120 + (i % 3) * 40) as usize;
        let mut buf: Vec<u8> = (0..n).map(|_| r.next() as u8).collect();
        if n >= 4 && i % 2 == 1 {
            let w = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
            let w = (w & 0xf0ff_c000) | (n as u32).wrapping_sub(16);
            buf[..4].copy_from_slice(&w.to_le_bytes());
        }
        let (fs, ls) = (r.next() & 3 != 0, r.next() & 3 != 0);
        let rx = RxIn {
            data: &buf,
            fs,
            ls,
            len: Some(n as u32 + 4),
        };
        rx_parse(&mut rnd, rx, &mut ops);
        beacon_parse(&buf);
        wlan_to_eth(&buf);
        mgmt_parse(&buf);
        hdr_parse(&buf);
    }
    rnd.rx_flush(&mut ops);
}
