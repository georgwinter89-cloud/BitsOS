//! The station's life cycle on V1's fake chip and its scripted AP (js/sta-testflow.mjs and
//! sta-testlink.mjs ported: there the station ran on stand-ins of the other modules, here every
//! module is the Rust one and the fake AP is the JS TestAp, driven through fake-host.mjs):
//! add_interface, the scan, connect (every frame with a PPDU status), data and mac80211's RX
//! checks, group rekeys, the coex act1 work, beacon loss and RSSI offload, BTC C2Hs, disconnect,
//! reconnect (a retransmitted message 1, data before the port opens), the AP's deauthentication,
//! a refused authentication, a deauthentication during the handshake, a BSS that needs PMF, a
//! channel the country forbids, remove_interface while connected, and add_interface again. H2Cs
//! are read back from what the station wrote into the CH12 buffers.

use rtw89_core::btc::BtcState;
use rtw89_core::bus::field;
use rtw89_core::chan::ChanState;
use rtw89_core::macinit;
use rtw89_core::phy::io::BB;
use rtw89_core::rfk1::RfkState;
use rtw89_core::rfk2::Rfk2State;
use rtw89_core::sta::io::Chip;
use rtw89_core::sta::link::{self, LinkError};
use rtw89_core::sta::scan::{self, ScanChan, ScanReq};
use rtw89_core::sta::{self as rsta, LinkState, Sta, StaEvent};
use rtw89_core::tables::regs::*;
use rtw89_core::txrx::Txrx;

use wifi::eapol::Supplicant;
use wifi::mlme::{JoinFailure, JoinTiming, Phase, reason};
use wifi::rsn::{self, Unusable};
use wifi::scan::Bss;

use super::sta::{FAKE_PASS, GoldenEnv, RecBus, SSID, hex};
use super::*;

const BSSID: [u8; 6] = [0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0x06];

/// One H2C as the station wrote it into a CH12 buffer
#[derive(Debug, Clone)]
pub(super) struct Seen {
    pub(super) cat: u32,
    pub(super) cls: u32,
    pub(super) func: u32,
    pub(super) payload: Vec<u8>,
}

impl Seen {
    pub(super) fn is(&self, k: (u32, u32, u32)) -> bool {
        (self.cat, self.cls, self.func) == k
    }
    pub(super) fn dw(&self, i: usize) -> u32 {
        u32::from_le_bytes(self.payload[i * 4..i * 4 + 4].try_into().unwrap())
    }
}

pub(super) const ADDR_CAM: (u32, u32, u32) = (
    H2C_CAT_MAC,
    H2C_CL_MAC_ADDR_CAM_UPDATE,
    H2C_FUNC_MAC_ADDR_CAM_UPD,
);
pub(super) const EDCA: (u32, u32, u32) = (H2C_CAT_MAC, H2C_CL_MAC_FW_OFLD, H2C_FUNC_USR_EDCA);
pub(super) const SEC: (u32, u32, u32) = (H2C_CAT_MAC, H2C_CL_MAC_SEC_CAM, H2C_FUNC_MAC_SEC_UPD);
pub(super) const OFLD: (u32, u32, u32) = (H2C_CAT_MAC, H2C_CL_MAC_FW_OFLD, H2C_FUNC_PACKET_OFLD);
pub(super) const DROP: (u32, u32, u32) = (H2C_CAT_MAC, H2C_CL_MAC_FW_OFLD, H2C_FUNC_PKT_DROP);
pub(super) const RSSI: (u32, u32, u32) = (H2C_CAT_MAC, H2C_CL_MAC_FW_OFLD, H2C_FUNC_OFLD_RSSI);
pub(super) const BCNFLTR: (u32, u32, u32) = (H2C_CAT_MAC, H2C_CL_MAC_FW_OFLD, H2C_FUNC_CFG_BCNFLTR);
pub(super) const CCTL: (u32, u32, u32) =
    (H2C_CAT_MAC, H2C_CL_MAC_FR_EXCHG, H2C_FUNC_MAC_CCTLINFO_UD);
pub(super) const JOIN: (u32, u32, u32) = (H2C_CAT_MAC, H2C_CL_MAC_MEDIA_RPT, H2C_FUNC_MAC_JOININFO);
pub(super) const ROLE: (u32, u32, u32) = (
    H2C_CAT_MAC,
    H2C_CL_MAC_MEDIA_RPT,
    H2C_FUNC_MAC_FWROLE_MAINTAIN,
);
pub(super) const PAUSE: (u32, u32, u32) =
    (H2C_CAT_MAC, H2C_CL_MAC_FW_OFLD, H2C_FUNC_MAC_MACID_PAUSE);
pub(super) const RA: (u32, u32, u32) = (
    H2C_CAT_OUTSRC,
    H2C_CL_OUTSRC_RA,
    H2C_FUNC_OUTSRC_RA_MACIDCFG,
);
pub(super) const GPKT: (u32, u32, u32) = (H2C_CAT_MAC, H2C_CL_FW_INFO, H2C_FUNC_MAC_GENERAL_PKT);
/// the station's own H2Cs (not chan's, btc's or rfk's)
const STA_H2C: [(u32, u32, u32); 13] = [
    ADDR_CAM, EDCA, SEC, OFLD, DROP, RSSI, BCNFLTR, CCTL, JOIN, ROLE, PAUSE, RA, GPKT,
];

pub(super) struct Life {
    pub(super) fake: Fake,
    pub(super) run: Bringup,
    pub(super) bst: BtcState,
    pub(super) rfk: RfkState,
    pub(super) rfk2: Rfk2State,
    pub(super) cs: ChanState,
    pub(super) txrx: Txrx,
    pub(super) sta: Sta,
    pub(super) env: GoldenEnv,
    pub(super) bf: bool,
    pub(super) writes: Vec<(usize, u32, Vec<u8>)>,
    /// which chip accesses fail (fault tests)
    pub(super) chip_fail: super::sta::Fail,
}

impl Life {
    pub(super) fn new(name: &str) -> Life {
        Life::with_flags(name, &["--sta"])
    }

    /// With other flags for fake-host.mjs (`--lan <port>`)
    pub(super) fn with_flags(name: &str, flags: &[&str]) -> Life {
        let (fake, run, hooks) = run_to_phy_stage_with(name, flags);
        let GoldenHooks {
            btc: mut bst,
            rfk,
            mut rfk2,
            chan: mut cs,
            ..
        } = hooks;
        bst.take_events();
        cs.take_events();
        let fw = Firmware::parse(firmware(), 1, false).unwrap();
        rfk2.load_txpwr_trk(&fw, 0).unwrap();
        let mut sta = Sta::new();
        rsta::track::track_start(&mut sta, 0);
        Life {
            fake,
            run,
            bst,
            rfk,
            rfk2,
            cs,
            txrx: Txrx::new(),
            sta,
            env: GoldenEnv {
                now: 0,
                rnd: 9,
                real: None,
            },
            bf: fw.has_feat_name("BEACON_FILTER"),
            writes: Vec::new(),
            chip_fail: super::sta::Fail::None,
        }
    }

    pub(super) fn go<R>(
        &mut self,
        f: impl FnOnce(&mut RecBus, &mut Chip, &mut Sta, &mut GoldenEnv) -> R,
    ) -> R {
        let mut bus = RecBus {
            inner: &mut self.fake.bus,
            writes: Vec::new(),
            fail: self.chip_fail,
        };
        let mut chip = Chip {
            run: &mut self.run,
            btc: &mut self.bst,
            rfk: &mut self.rfk,
            rfk2: &mut self.rfk2,
            chan: &mut self.cs,
            txrx: &mut self.txrx,
            beacon_filter: self.bf,
        };
        let r = f(&mut bus, &mut chip, &mut self.sta, &mut self.env);
        self.writes.extend(bus.writes);
        r
    }

    /// One line to the fake host, after it handled everything sent
    pub(super) fn ask(&mut self, q: &str) -> String {
        self.fake.bus.sync().unwrap();
        self.fake.ask(q)
    }

    pub(super) fn mark(&self) -> (usize, usize) {
        (self.writes.len(), self.fake.bus.lines().len())
    }

    /// Every H2C written since `m`
    pub(super) fn h2cs_all(&self, m: (usize, usize)) -> Vec<Seen> {
        let ch12 = &self.run.rings.as_ref().unwrap().tx("CH12").unwrap().bufs;
        self.writes[m.0..]
            .iter()
            .filter(|(_, a, b)| ch12.contains(a) && b.len() >= 32)
            .map(|(_, _, b)| {
                let w0 = u32::from_le_bytes(b[24..28].try_into().unwrap());
                let w1 = u32::from_le_bytes(b[28..32].try_into().unwrap());
                let len = (field(w1, H2C_HDR_TOTAL_LEN) as usize).min(b.len() - 24);
                Seen {
                    cat: field(w0, H2C_HDR_CAT),
                    cls: field(w0, H2C_HDR_CLASS),
                    func: field(w0, H2C_HDR_FUNC),
                    payload: b[32..24 + len.max(8)].to_vec(),
                }
            })
            .collect()
    }

    /// The station's H2Cs since `m` (not chan's, btc's or rfk's)
    pub(super) fn h2cs(&self, m: (usize, usize)) -> Vec<Seen> {
        self.h2cs_all(m)
            .into_iter()
            .filter(|s| STA_H2C.iter().any(|&k| s.is(k)))
            .collect()
    }

    pub(super) fn names(&self, m: (usize, usize)) -> Vec<&'static str> {
        self.h2cs(m).iter().map(name).collect()
    }

    pub(super) fn lines(&self, m: (usize, usize)) -> Vec<String> {
        self.fake.bus.lines()[m.1..].to_vec()
    }

    pub(super) fn own(&self) -> [u8; 6] {
        self.run.phy.efuse.as_ref().unwrap().addr
    }

    pub(super) fn connect(&mut self, bss: &Bss) -> Result<link::Joined, LinkError> {
        let own = self.own();
        let pmk = wifi::crypto::psk(FAKE_PASS, SSID).unwrap();
        let own_ie = rsn::own_ie(bss.group_cipher());
        let sup = Supplicant::new(pmk, own, bss.bssid, &own_ie, bss.rsn_ie.as_deref()).unwrap();
        self.go(|b, c, s, e| link::connect(b, c, s, e, bss, sup, JoinTiming::default()))
    }

    pub(super) fn poll(&mut self) -> Vec<Vec<u8>> {
        self.go(|b, c, s, e| link::poll(b, c, s, e))
            .expect("poll")
            .eth
    }

    pub(super) fn ap(&mut self, q: &str) -> String {
        let a = self.ask(&format!("ap {q}"));
        assert!(!a.starts_with("unknown"), "{q}: {a}");
        a
    }

    pub(super) fn ap_log(&mut self) -> Vec<String> {
        let a = self.ask("ap-log");
        a.trim_start_matches("ap-log ")
            .split(',')
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect()
    }

    /// The key bytes of the SEC_CAM H2Cs since `m`
    pub(super) fn sec_keys(&self, m: (usize, usize)) -> Vec<String> {
        self.h2cs(m)
            .iter()
            .filter(|s| s.is(SEC))
            .map(|s| hex(&s.payload[8..24]))
            .collect()
    }
}

fn name(s: &Seen) -> &'static str {
    let k = (s.cat, s.cls, s.func);
    let n = [
        (ADDR_CAM, "ADDR_CAM"),
        (EDCA, "EDCA"),
        (SEC, "SEC"),
        (OFLD, "OFLD"),
        (DROP, "DROP"),
        (RSSI, "RSSI"),
        (BCNFLTR, "BCNFLTR"),
        (CCTL, "CCTL"),
        (JOIN, "JOIN"),
        (ROLE, "ROLE"),
        (PAUSE, "PAUSE"),
        (RA, "RA"),
        (GPKT, "GPKT"),
    ];
    n.iter().find(|(x, _)| *x == k).map_or("?", |(_, n)| n)
}

/// RTW89_SET_EDCA_AC and _PARAM of an EDCA H2C
fn edca(s: &Seen) -> (u32, u32) {
    (field(s.dw(0), 0x60), s.dw(1))
}

fn rep(n: usize, x: &'static str) -> Vec<&'static str> {
    vec![x; n]
}

fn c2h_bytes(cat: u32, cls: u32, func: u32, w2: u32) -> String {
    let mut b = Vec::new();
    b.extend_from_slice(&(cat | (cls << 2) | (func << 8)).to_le_bytes());
    b.extend_from_slice(&12u32.to_le_bytes());
    b.extend_from_slice(&w2.to_le_bytes());
    hex(&b)
}

#[test]
fn sta_life_cycle_on_the_fake_ap() {
    if skip("sta_life_cycle_on_the_fake_ap") {
        return;
    }
    let mut l = Life::new("life");
    let own = l.own();

    // add_interface: rtw89_mac_vif_init's H2Cs, then mac80211's interface start: the ERP reset
    // (conf_tx with the zeroed tx_params), the default EDCA (AC VO, VI, BE, BK)
    let m = l.mark();
    l.go(|b, c, s, e| rsta::add_interface(b, c, s, e, own))
        .unwrap();
    assert_eq!(
        l.names(m),
        [
            vec!["PAUSE", "ROLE", "JOIN", "ADDR_CAM", "CCTL"],
            rep(8, "EDCA")
        ]
        .concat()
    );
    let ed: Vec<(u32, u32)> = l.h2cs(m).iter().filter(|s| s.is(EDCA)).map(edca).collect();
    assert_eq!(
        ed,
        [
            (3, 0xa),
            (2, 0xa),
            (0, 0xa),
            (1, 0xa),
            (3, 0xa432),
            (2, 0xa432),
            (0, 0xa432),
            (1, 0xa432)
        ]
    );
    assert_eq!(l.run.mac.rx_fltr, macinit::default_rx_fltr());
    assert_eq!(
        (l.sta.state, l.sta.vif.mac_id, l.sta.vif.port),
        (LinkState::Up, 0, 0)
    );
    assert!(
        l.go(|b, c, s, e| rsta::add_interface(b, c, s, e, own))
            .is_err(),
        "twice: refused"
    );

    // the software scan over channels 1, 6, 11: scan start (addr CAM with the scan address), the
    // flush, the scan filter, a probe per channel; RX interrupt mitigation off and EDCCA to its
    // maximum, restored after
    let m = l.mark();
    let req = ScanReq::new(Band::B2g, &[1, 6, 11], 15);
    let list = l.go(|b, c, s, e| scan::scan(b, c, s, e, &req)).unwrap();
    assert_eq!(
        l.names(m),
        ["ADDR_CAM", "ADDR_CAM"],
        "scan start / complete"
    );
    let bss = list
        .iter()
        .find(|b| b.bssid == BSSID)
        .cloned()
        .expect("the AP");
    assert_eq!(
        (bss.ssid.as_slice(), bss.ch, bss.dtim_period),
        (SSID, 6, Some(3))
    );
    assert!(
        l.ap_log().iter().any(|x| x == "probe"),
        "a probe request on its channel"
    );
    let lines = l.lines(m);
    let int_mit = format!("wr32 0x{:08X} 0x00000000 0x00000000", R_AX_INT_MIT_RX);
    assert!(
        lines.contains(&int_mit),
        "no RX interrupt mitigation while scanning"
    );
    let ed_lvl = format!("0x{:08X}", BB + R_SEG0R_EDCCA_LVL_V1);
    let edcca: Vec<&String> = lines.iter().filter(|x| x.contains(&ed_lvl)).collect();
    assert_eq!(
        edcca.len(),
        9,
        "3 reads, 3 writes of EDCCA_MAX, 3 restores: {edcca:?}"
    );
    assert!(
        edcca[3].ends_with("0x000000F9") && edcca[5].ends_with("0xF9000000"),
        "{edcca:?}"
    );
    // rtw89_pci_napi_poll: the release report queue is read before every RX queue poll
    let rpq = lines
        .iter()
        .filter(|x| x.starts_with("rd32 0x00001054 "))
        .count();
    let rxq = lines
        .iter()
        .filter(|x| x.starts_with("rd32 0x00001050 "))
        .count();
    assert!(rxq >= 3 && rpq == rxq, "{rpq} / {rxq}");
    assert!(!l.sta.scanning && l.run.phy.dig.bypass_dig);

    // DIG suspend/resume below must not restore bak_dig (rtw89_phy_dig_resume: restore false)
    l.run.phy.dig.bak_dig = 0x13;
    // connect: auth, assoc, the 4-way handshake, keys into the CAM (V1's fake AP beacons only
    // while the station scans, so no RSSI offload here: that comes with the beacons below)
    let m = l.mark();
    let j = l.connect(&bss).unwrap();
    assert_eq!((j.aid, j.group_key), (1, true));
    let log = l.ap_log();
    let no_ofld: Vec<&String> = log.iter().filter(|x| !x.starts_with("pktofld")).collect();
    let tail = &no_ofld[no_ofld.len().saturating_sub(6)..];
    assert_eq!(
        tail,
        ["auth", "assoc", "msg1", "msg2 ok", "msg3", "msg4 ok"]
    );
    // prep_connection (BSSID), sta_assoc's H2Cs, the packet offloads, the beacon filter, the ERP slot,
    // then the keys (SEC_CAM, ADDR_CAM each)
    let n = l.names(m);
    assert_eq!(
        n,
        [
            vec!["ADDR_CAM"],
            rep(4, "EDCA"),
            vec![
                "CCTL", "JOIN", "ADDR_CAM", "RA", "OFLD", "OFLD", "OFLD", "GPKT", "BCNFLTR"
            ],
            rep(4, "EDCA"),
            vec!["SEC", "ADDR_CAM", "SEC", "ADDR_CAM"]
        ]
        .concat()
    );
    let h = l.h2cs(m);
    let ed: Vec<(u32, u32)> = h.iter().filter(|s| s.is(EDCA)).map(edca).collect();
    assert_eq!(
        ed,
        [
            (3, 0xa432),
            (2, 0xa432),
            (0, 0xa432),
            (1, 0xa432),
            (3, 0xa41c),
            (2, 0xa41c),
            (0, 0xa41c),
            (1, 0xa41c)
        ],
        "DCF defaults with the long slot, then the AP's short slot"
    );
    let ap = l.sta.ap.clone().unwrap();
    assert_eq!(
        (ap.avg_rssi.read(), ap.rssi[0].read(), ap.rssi[1].read()),
        (0x64, 0x64, 0x60),
        "AP link RSSI averages"
    );
    let ra = h.iter().find(|s| s.is(RA)).unwrap();
    let w0 = ra.dw(0);
    assert_eq!(
        (
            field(w0, RTW89_H2C_RA_W0_MODE),
            field(w0, RTW89_H2C_RA_W0_INIT_RATE_LV),
            field(w0, RTW89_H2C_RA_W0_UPD_ALL),
            ra.dw(1)
        ),
        (3, 1, 1, 0xfe0),
        "RA: CCK+OFDM, init level 1, rate floor from avg_rssi"
    );
    let con = &h[h.iter().position(|s| s.is(JOIN)).unwrap() + 1];
    assert_eq!(
        (
            field(con.dw(9), ADDR_CAM_W9_AID12),
            field(con.dw(2), ADDR_CAM_W2_NET_TYPE)
        ),
        (1, 2),
        "CON_DISCONN: AID 1, INFRA"
    );
    let gp = h.iter().find(|s| s.is(GPKT)).unwrap();
    assert_eq!(hex(&gp.payload), "00ff000102ff", "general packet ids");
    let ofld = l.ap("ofld");
    let nul = format!("48010000{}{}{}0000", hex(&BSSID), hex(&own), hex(&BSSID));
    let ps = format!("a40001c0{}{}", hex(&BSSID), hex(&own));
    assert_eq!(
        ofld,
        format!("ap ofld 0:{ps},1:{nul},2:{nul} 0"),
        "the offloaded PS-Poll and null frames"
    );
    let pd = format!("rmw32 0x{:08X}", BB + R_SEG0R_PD_V1);
    let dig: Vec<String> = l
        .lines(m)
        .into_iter()
        .filter(|x| x.starts_with(&pd))
        .collect();
    assert_eq!(
        dig,
        [0x7c0, 0x4000_0000, 0x7c0, 0x4000_0000].map(|mk| format!("{pd} 0x{mk:08X} 0x00000000")),
        "DIG suspend in sta_add, resume in sta_assoc"
    );
    let keys = l.sec_keys(m);
    let tk = l.ap("tk").trim_start_matches("ap tk ").to_string();
    let gtk = l.ap("gtk");
    let gtk_hex = gtk.split(' ').nth(2).unwrap().to_string();
    assert_eq!(
        keys,
        [tk.clone(), gtk_hex.clone()],
        "the keys the authenticator derived"
    );
    let sec: Vec<(u32, u32)> = h
        .iter()
        .filter(|s| s.is(SEC))
        .map(|s| (s.dw(0), s.dw(1)))
        .collect();
    assert_eq!(
        sec,
        [(0x0014_0000, 6), (0x0014_0001, 6)],
        "SEC_CAM: idx, len 20, type CCMP128"
    );
    let ac = &l.sta.vif.addr_cam;
    assert_eq!(
        (
            ac.sec_cam_map,
            ac.sec_ent[0],
            ac.sec_ent[2],
            ac.sec_ent_keyid[2]
        ),
        (0b101, 0, 1, 1),
        "key slots 0 and 2"
    );
    assert_eq!(
        l.sta.dev.act1_at, None,
        "coex act1 work cancelled by EAPOL_END"
    );
    let assoc = l.ap("assoc");
    assert!(
        assoc.starts_with("ap assoc 2,4,11,22,12,18,24,36,48,72,96,108 30140100000fac040100000fac040100000fac020000"),
        "assoc request: our rates of the AP, our RSN element: {assoc}"
    );

    // data out: ARP with CCMP header, PN 1, key 0
    let mut arp = BSSID.to_vec();
    arp.extend_from_slice(&own);
    arp.extend_from_slice(&[0x08, 0x06]);
    arp.extend_from_slice(&[1; 28]);
    l.go(|b, c, s, e| link::send(b, c, s, e, &arp)).unwrap();
    let sent = l.ap("sent");
    assert!(
        sent.ends_with(" 1:0:1") && sent.contains("p:806:"),
        "{sent}"
    );
    // mac80211's RX checks: PN at the RSC of 3/4 (5), a replay, a plain frame, one the chip did not
    // decrypt are dropped; a unicast retransmission with the same sequence control is a duplicate,
    // a group-addressed one never is
    let p = |s: &str| hex(s.as_bytes());
    for q in [
        format!("data bc 0800 {} 1 1 5 1 0 -", p("old")),
        format!("data bc 0800 {} 1 1 6 1 0 -", p("hello from the AP")),
        format!("data bc 0800 {} 1 1 6 1 0 -", p("replayed")),
        format!("data bc 0800 {} 0 0 0 0 0 -", p("plain")),
        format!("data bc 0800 {} 1 1 7 0 0 -", p("not decrypted")),
        format!("data sta 0800 {} 1 0 2 1 0 -", p("unicast")),
        format!("data sta 0800 {} 1 0 3 1 1 last", p("retransmitted")),
        format!("data bc 0800 {} 1 1 8 1 1 last", p("group again")),
    ] {
        l.ap(&q);
    }
    l.sta.take_events();
    let got = l.poll();
    let text: Vec<String> = got
        .iter()
        .map(|e| String::from_utf8_lossy(&e[14..]).into_owned())
        .collect();
    assert_eq!(text, ["hello from the AP", "unicast", "group again"]);
    assert_eq!(&got[0][6..12], &BSSID, "source address of a From DS frame");
    let drops: Vec<String> = l
        .sta
        .take_events()
        .into_iter()
        .filter_map(|e| match e {
            StaEvent::DataDropped { why } => Some(why.as_str().to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(
        drops,
        [
            "replay",
            "replay",
            "unencrypted with keys in",
            "not decrypted"
        ]
    );

    // an EAPOL frame not to us (group-addressed) never reaches the supplicant (control port)
    l.ap(&format!("data bc 888e {} 1 1 9 1 0 -", hex(&[1, 3, 0, 95])));
    assert!(l.poll().is_empty());
    let ev = l.sta.take_events();
    assert!(
        !ev.iter().any(|e| matches!(
            e,
            StaEvent::Wifi(
                wifi::WifiEvent::EapolTaken { .. } | wifi::WifiEvent::EapolDropped { .. }
            )
        )),
        "{ev:?}"
    );

    // group rekey: the new GTK (key ID 2), then group 2/2 out; the coex act1 work queued by that
    // EAPOL runs at the first poll a second later
    let m = l.mark();
    l.ap("rekey 62626262626262626262626262626262");
    l.poll();
    assert_eq!(l.ap_log().last().unwrap(), "group2 ok");
    assert_eq!(l.names(m), ["SEC", "ADDR_CAM"]);
    assert_eq!(l.sec_keys(m), ["62626262626262626262626262626262"]);
    assert!(l.sta.keys.gtk(2).is_some() && l.sta.keys.gtk(1).is_some());
    assert!(
        l.sta.dev.act1_at.is_some_and(|t| t > l.env.now),
        "act1 work queued"
    );
    l.env.now += 1000;
    let lines_before = l.fake.bus.lines().len();
    l.poll();
    assert_eq!(
        l.sta.dev.act1_at, None,
        "rtw89_coex_act1_work ran at the next poll"
    );
    assert!(l.fake.bus.lines().len() > lines_before);
    // the GTK in force once more: answered, not installed again (KRACK guard)
    let m = l.mark();
    l.ap("rekey -");
    l.poll();
    assert_eq!(l.ap_log().last().unwrap(), "group2 ok");
    assert!(
        l.names(m).is_empty(),
        "no SEC_CAM for a key in force: {:?}",
        l.names(m)
    );

    // the firmware's beacon loss report: noted, the filter armed again; a BTC report goes to btc;
    // beacons of our BSS give an RSSI offload, of another BSS nothing
    let m = l.mark();
    let loss = c2h_bytes(
        RTW89_C2H_CAT_MAC,
        RTW89_MAC_C2H_CLASS_OFLD,
        RTW89_MAC_C2H_FUNC_BCNFLTR_RPT,
        l.sta.vif.mac_id | (RTW89_BCN_FLTR_BEACON_LOSS << 8),
    );
    l.ap(&format!("c2h {loss}"));
    l.ap(&format!(
        "c2h {}",
        c2h_bytes(RTW89_C2H_CAT_OUTSRC, BTFC_FW_EVENT, BTF_EVNT_RPT, 0)
    ));
    l.ap("beacon");
    l.ap("beacon 02aabbccdd07");
    l.poll();
    assert!(l.sta.beacon_loss, "beacon loss noted");
    assert_eq!(l.names(m), ["BCNFLTR", "RSSI"]);
    let rs = l.h2cs(m).into_iter().find(|s| s.is(RSSI)).unwrap();
    assert_eq!(
        (rs.dw(0), rs.dw(1)),
        (1 << 8, 0),
        "RSSI offload: MAC ID 0, NUM 1, the beacon's rssi_avg"
    );
    l.sta.beacon_loss = false;

    // disconnect (ieee80211_set_disassoc, with our deauthentication): dropping flush (4 PKT_DROP),
    // deauth, flush; the AP's station destroyed; ASSOC off, BSSID and ERP slot, default EDCA;
    // cfg80211 deletes the group keys
    let m = l.mark();
    l.go(|b, c, s, e| link::disconnect(b, c, s, e, reason::DEAUTH_LEAVING))
        .unwrap();
    assert!(l.ap_log().iter().any(|x| x == "deauth 3"));
    let n = l.names(m);
    assert_eq!(
        n,
        [
            rep(4, "DROP"),
            vec![
                "ADDR_CAM", "BCNFLTR", "OFLD", "OFLD", "OFLD", "CCTL", "JOIN", "ADDR_CAM",
                "ADDR_CAM"
            ],
            rep(4, "EDCA"),
            rep(4, "EDCA"),
            vec!["ADDR_CAM", "ADDR_CAM"]
        ]
        .concat()
    );
    let h = l.h2cs(m);
    let drops: Vec<Vec<u32>> = h
        .iter()
        .filter(|s| s.is(DROP))
        .map(|s| (0..6).map(|i| s.dw(i)).collect())
        .collect();
    assert_eq!(
        drops,
        (0..4)
            .map(|sel| vec![sel, 0, 0, 0, 0, 0])
            .collect::<Vec<_>>(),
        "PKT_DROP: MACID_BE/BK/VI/VO_ONCE"
    );
    let ed: Vec<(u32, u32)> = h.iter().filter(|s| s.is(EDCA)).map(edca).collect();
    assert_eq!(
        ed,
        [3, 2, 0, 1, 3, 2, 0, 1].map(|a| (a, 0xa432)),
        "EDCA after the disassociation: long slot"
    );
    let bf = format!(
        "rmw32 0x{:08X} 0x00000007 0x00000000",
        R_AX_BFMEE_RESP_OPTION
    );
    assert!(l.lines(m).contains(&bf), "BFee NDPA off");
    assert_eq!(
        (
            l.sta.vif.addr_cam.sec_cam_map,
            l.sta.dev.pkt_offload_map.iter().filter(|&&b| b).count(),
            l.sta.vif.net_type,
            l.sta.vif.use_short_slot
        ),
        (0, 0, RTW89_NET_TYPE_NO_LINK, false)
    );
    assert_eq!(l.ap("ofld"), "ap ofld - 0", "the offloads deleted");
    assert!(
        l.sta
            .mlme
            .as_ref()
            .is_some_and(|m| m.phase() == Phase::Down),
        "the link's context closed"
    );

    // again, now the AP's 1/4 also comes as a retransmission and data comes before the port opens
    l.ap("set dupMsg1 1");
    l.ap("set earlyData 1");
    let before = l.ap_log().iter().filter(|x| *x == "msg2 ok").count();
    let m = l.mark();
    l.connect(&bss).unwrap();
    l.ap("set dupMsg1 0");
    l.ap("set earlyData 0");
    assert_eq!(
        l.ap_log().iter().filter(|x| *x == "msg2 ok").count() - before,
        1,
        "the retransmitted 1/4 is not answered twice"
    );
    assert_eq!(l.sta.vif.addr_cam.sec_cam_map, 0b101);
    let ed: Vec<(u32, u32)> = l.h2cs(m).iter().filter(|s| s.is(EDCA)).map(edca).collect();
    assert_eq!(
        ed[4..],
        [3, 2, 0, 1].map(|a| (a, 0xa41c)),
        "ERP short slot announced again"
    );
    assert!(
        l.poll().is_empty(),
        "the data frame before the port was authorized is not delivered"
    );
    // the AP's broadcast deauthentication: no frame of ours, no dropping flush, the same teardown
    let m = l.mark();
    l.ap("deauth 7 bc");
    l.poll();
    assert_eq!(
        (
            l.sta.state,
            l.sta.last_reason,
            l.sta.ap.is_none(),
            l.sta.keys.ptk.is_none(),
            l.sta.keys.gtk.len()
        ),
        (LinkState::Up, 7, true, true, 0)
    );
    assert!(!l.names(m).contains(&"DROP"));
    assert_eq!(
        l.ap_log()
            .iter()
            .filter(|x| x.starts_with("deauth"))
            .count(),
        1,
        "no deauthentication of ours"
    );

    // an AP that refuses the authentication: connect fails, station entry and BSSID gone
    l.ap("set refuseAuth 1");
    let m = l.mark();
    let e = l.connect(&bss).unwrap_err();
    l.ap("set refuseAuth 0");
    assert_eq!(e, LinkError::Join(JoinFailure::AuthRefused { status: 1 }));
    assert_eq!(e.to_string(), "authentication refused, status 1");
    let n = l.names(m);
    assert!(
        !n.contains(&"EDCA") && !n.contains(&"SEC") && n.last() == Some(&"ADDR_CAM"),
        "{n:?}"
    );
    assert!(l.sta.ap.is_none() && l.sta.vif.bssid == [0; 6]);

    // the AP deauthenticates during the 4-way handshake (reason 2): the link ends, no frame of ours
    l.ap("set deauthAfter2 1");
    let m = l.mark();
    let e = l.connect(&bss).unwrap_err();
    l.ap("set deauthAfter2 0");
    assert_eq!(
        e.to_string(),
        "deauthenticated by the AP during the handshake, reason 2"
    );
    assert!(l.sta.ap.is_none() && !l.sta.vif.assoc && !l.names(m).contains(&"DROP"));

    // a BSS connect refuses (PMF required): nothing done at all
    let mut pmf = bss.clone();
    pmf.rsn_ie = Some(hex_to("30140100000fac040100000fac040100000fac02c000"));
    let m = l.mark();
    assert_eq!(
        l.connect(&pmf).unwrap_err(),
        LinkError::Unusable(Unusable::PmfRequired)
    );
    assert_eq!(l.fake.bus.lines().len(), m.1, "nothing sent");
    // a channel the country (DE) forbids: refused before the scan starts
    let req = ScanReq {
        band: Band::B2g,
        channels: vec![
            ScanChan {
                ch: 1,
                passive: false,
            },
            ScanChan {
                ch: 14,
                passive: false,
            },
        ],
        dwell_ms: 1,
        ssids: vec![Vec::new()],
    };
    assert!(l.go(|b, c, s, e| scan::scan(b, c, s, e, &req)).is_err());
    assert_eq!(l.fake.bus.lines().len(), m.1, "nothing touched");

    // remove_interface while connected: cfg80211 leaves the BSS first, then rtw89_mac_vif_deinit
    l.connect(&bss).unwrap();
    let deauths = l.ap_log().iter().filter(|x| *x == "deauth 3").count();
    let m = l.mark();
    l.go(|b, c, s, e| rsta::remove_interface(b, c, s, e))
        .unwrap();
    assert_eq!(
        l.ap_log().iter().filter(|x| *x == "deauth 3").count(),
        deauths + 1,
        "disconnected first"
    );
    let h = l.h2cs(m);
    let n: Vec<&str> = h.iter().map(name).collect();
    assert_eq!(n[n.len() - 2..], ["ROLE", "ADDR_CAM"]);
    let last = h.last().unwrap();
    assert_eq!(
        (
            field(last.dw(2), ADDR_CAM_W2_VALID),
            field(last.dw(13), ADDR_CAM_W13_BSSID_VALID)
        ),
        (0, 0)
    );
    assert_eq!(l.sta.state, LinkState::Down);
    // add again: rtwdev state (CAM maps, MAC IDs) carried over, not reset
    l.go(|b, c, s, e| rsta::add_interface(b, c, s, e, own))
        .unwrap();
    assert_eq!(
        (
            l.sta.vif.mac_id,
            l.sta.vif.addr_cam.idx,
            l.sta.dev.total_sta_assoc
        ),
        (0, 0, 0)
    );
    assert!(
        l.sta.dev.cam.sec_in_use() == 0,
        "no key left in the security CAM"
    );
    let _ = l.fake.finish();
}

fn hex_to(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}
