//! test-btc.mjs in Rust (no node): the register order of the init and run sequences against the C
//! (mac.c, rtw8852b_common.c, coex.c), the H2C byte layouts against fw.c / coex.c structs, the
//! firmware's coex reports (coex.c _chk_btc_report), on `rfk1::fake::RfFake` as test-btc.mjs's
//! FakeDev; plus the events, the refusals with their values and the H2C path to CH12.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use super::*;
use crate::bus::{field, prep};
use crate::fw::hookbus::HookBus;
use crate::membus::MemBus;
use crate::phy::io::BB;
use crate::rfk1::fake::{Key, RfFake};
use crate::rfk1::util::btc_phymap;
use crate::tables::BtcSlot;
use crate::tables::btc::{S_DEF, act, wlink};

/// RTW89_FW_VER_CODE of the Mini-PC's firmware 0.29.29.18
pub(super) const FW_CODE: u32 = 0x001d_1d12;

/// The coex H2Cs, each with its position in the trace (as test-btc.mjs reads them off CH12)
#[derive(Default)]
pub(super) struct Rec {
    pub h2c: Vec<(usize, H2c, Vec<u8>)>,
}

impl BtcHooks<RfFake> for Rec {
    fn h2c(&mut self, bus: &mut RfFake, h: &H2c, payload: &[u8]) -> Rtw89Result<()> {
        self.h2c.push((bus.trace.len(), *h, payload.to_vec()));
        Ok(())
    }
}

pub(super) fn chip(cmac_on: bool) -> RfFake {
    let mut b = RfFake::new();
    if cmac_on {
        b.answer(Key::Mac(R_AX_CMAC_FUNC_EN), B_AX_CMAC_EN);
    }
    b
}

pub(super) const RFE1: ChipFacts = ChipFacts { rfe: 1, cv: 1 };

pub(super) fn inited(b: &mut RfFake, r: &mut Rec) -> BtcState {
    let mut st = BtcState::new(FW_CODE).unwrap();
    power_on(&mut st);
    init(b, &mut st, r, RFE1, BTC_MODE_NORMAL).unwrap();
    st
}

// ---- expected ops, written from the C (test-btc.mjs `op`) ----

fn wr(sz: u32, a: u32, v: u32) -> String {
    format!("wr{} 0x{a:08X} 0x00000000 0x{v:08X}", sz * 8)
}
fn rd(sz: u32, a: u32) -> String {
    format!("rd{} 0x{a:08X} 0x00000000 0x00000000", sz * 8)
}
fn rmw(sz: u32, a: u32, clr: u32, set: u32) -> String {
    format!("rmw{} 0x{a:08X} 0x{clr:08X} 0x{set:08X}", sz * 8)
}
fn poll(sz: u32, a: u32, m: u32, v: u32, ms: u32) -> String {
    format!("poll{} 0x{a:08X} 0x{m:08X} 0x{v:08X} {ms} ms", sz * 8)
}
fn delay(us: u32) -> String {
    format!("delay 0x{us:08X} 0x00000000 0x00000000")
}
fn h2c_mark(func: u32) -> String {
    format!("h2c {func}")
}

const LTE_CTRL: u32 = 0xDAF0;
fn lte_w(off: u32, v: u32) -> Vec<String> {
    vec![
        poll(1, LTE_CTRL + 3, 0x20, 0x20, 50),
        wr(4, 0xDAF4, v),
        wr(4, LTE_CTRL, 0xC00F_0000 | off),
    ]
}
// rtw89_phy_write_rf_a (SWSI) with the full RF mask: busy poll, one data word
fn rf(path: u32, addr: u32, data: u32) -> Vec<String> {
    vec![
        poll(4, BB + R_SWSI_V1, B_SWSI_W_BUSY_V1 | B_SWSI_R_BUSY_V1, 0, 1),
        wr(
            4,
            BB + R_SWSI_DATA_V1,
            prep(B_SWSI_DATA_PATH_V1, path)
                | prep(B_SWSI_DATA_ADDR_V1, addr)
                | prep(B_SWSI_DATA_VAL_V1, data),
        ),
    ]
}
fn trx_mask(path: u32, group: u32, val: u32) -> Vec<String> {
    [
        rf(path, RR_LUTWE, 0x20000),
        rf(path, RR_LUTWA, group),
        rf(path, RR_LUTWD0, val),
        rf(path, RR_LUTWE, 0),
    ]
    .concat()
}
// __rtw8852bx_btc_init_cfg with rtw89_mac_coex_init (RTK mode, inner)
fn init_cfg() -> Vec<String> {
    [
        vec![
            rmw(1, 0x40, 0, B_AX_ENBT),
            rmw(1, 0xDA20, 0, B_AX_PTA_WL_TX_EN),
            rmw(1, 0xDA35, 0, 0x01),
            rmw(1, 0xDA40, 0, 0x0C),
            rmw(1, 0xDA42, 0, 0x01),
            rmw(1, 0xCC07, 0x02, 0),
            rmw(2, 0xC340, 0x220, 0x20),
            poll(1, LTE_CTRL + 3, 0x20, 0x20, 50),
            wr(4, LTE_CTRL, 0x800F_003C),
            rd(4, 0xDAF8),
        ],
        lte_w(0x3C, 0),
        vec![
            rmw(1, 0x40, 0xC0, 0),
            rmw(1, 0xDA4C, 0, 0x01),
            rmw(1, 0xDA6C, 0x3F, 0x05),
            rmw(1, 0x41, 0x06, 0x02),
            rmw(4, 0xDA30, 0, 1 << 3),
            rmw(4, 0xDA10, 0, 1 << 8),
        ],
        rf(0, 0x02, 0),
        rf(1, 0x02, 0),
        trx_mask(0, 0, 0x5ff),
        trx_mask(1, 0, 0x5ff),
        trx_mask(0, 2, 0x5ff),
        trx_mask(1, 2, 0x55f),
        vec![
            wr(4, 0xDA2C, 0xf0ff_ffff),
            rmw(4, 0xDA40, 0, (1 << 16) | (1 << 2)),
        ],
    ]
    .concat()
}
/// S0/S1: SW control of both, value WL (or BT)
const GNT_WL: u32 = 0x7700_7700;
const GNT_BT: u32 = 0xDD00_DD00;
fn preagc_dis() -> Vec<String> {
    let rows: [(u32, u32, u32); 14] = [
        (0x46D0, 3, 0),
        (0x4790, 3, 0),
        (0x4AD4, 0xffff_ffff, 0x60),
        (0x4AE0, 0xffff_ffff, 0x60),
        (0x4688, 0xff00_0000, 0x1a),
        (0x476C, 0xff00_0000, 0x1a),
        (0x4694, 0xff, 0x2a),
        (0x4694, 0xff00, 0x2a),
        (0x4778, 0xff, 0x2a),
        (0x4778, 0xff00, 0x2a),
        (0x4AE4, 0xff_ffff, 0x79E99E),
        (0x4AEC, 0xff_ffff, 0x79E99E),
        (0x469C, 0xfc00_0000, 0x26),
        (0x49F0, 0xfc00_0000, 0x26),
    ];
    rows.iter()
        .map(|&(a, m, v)| {
            if m == 0xffff_ffff {
                wr(4, BB + a, v)
            } else {
                rmw(4, BB + a, m, prep(m, v))
            }
        })
        .collect()
}
fn ant_wl(by_wl: bool) -> Vec<String> {
    let mut v = lte_w(0x38, if by_wl { GNT_WL } else { GNT_BT });
    v.push(if by_wl {
        rmw(1, 0x73, 0, 4)
    } else {
        rmw(1, 0x73, 4, 0)
    });
    v.push(rd(4, 0xC000));
    v.push(wr(2, 0xC67C, 0x111));
    v
}
fn sb(drv: u32) -> Vec<String> {
    vec![rmw(4, 0xAC, 0x81ff_ffff, 0x8100_0000 | drv), delay(1000)]
}
const SET_DRV: u32 = SET_DRV_INFO;
const SET_SLOT: u32 = SET_SLOT_TABLE;
const SET_MREG: u32 = SET_MREG_TABLE;
const SET_POLICY: u32 = SET_CX_POLICY;
const SET_RPT: u32 = SET_REPORT_EN;

/// The trace from op `from` with each H2C as one `h2c <func>` line where it was sent
fn norm(b: &RfFake, r: &Rec, from: usize) -> Vec<String> {
    let lines = b.lines();
    let mut out = Vec::new();
    for i in from..=lines.len() {
        for (pos, h, _) in &r.h2c {
            if *pos == i {
                out.push(h2c_mark(h.func));
            }
        }
        if let Some(l) = lines.get(i) {
            out.push(l.clone());
        }
    }
    out
}

#[track_caller]
fn same(got: &[String], want: &[String], what: &str) {
    for (i, (g, w)) in got.iter().zip(want).enumerate() {
        assert_eq!(g, w, "{what}: op {i}");
    }
    assert_eq!(got.len(), want.len(), "{what}: op count");
}

fn slot(s: &BtcSlot) -> Vec<u8> {
    let mut b = vec![0u8; 8];
    h2c::put_slot(&mut b, s);
    b
}
fn tdma_tlv(bind: u8, opt: u8) -> Vec<u8> {
    vec![0, 12, 3, 0, 0, 0, 0, 0, 0, bind, 0, 0, 0, opt]
}
fn slot_tlv(sid: u8, s: &BtcSlot) -> Vec<u8> {
    [vec![1, 10, 1, sid], slot(s)].concat()
}
const OFF_BT_SLOT: BtcSlot = BtcSlot {
    dur: 100,
    cxtbl: 0xe555_5555,
    cxtype: 0,
};
fn full_policy(off: BtcSlot) -> Vec<u8> {
    let mut v = tdma_tlv(1, 1);
    for (i, s) in S_DEF.iter().enumerate() {
        v.extend(slot_tlv(i as u8, if i == 0 { &off } else { s }));
    }
    v
}
fn rpt_cmds() -> Vec<String> {
    (0..14).map(|_| h2c_mark(SET_RPT)).collect()
}

#[test]
fn the_version_of_firmware_0_29_29_is_recognized() {
    let st = BtcState::new(FW_CODE).unwrap();
    assert_eq!(
        (st.ver.fcxbtcrpt, st.ver.fwlrole, st.ver.fcxslots),
        (105, 2, 1)
    );
    assert!(st.wl.status.init_ok);
    for code in [0x001d_1cff, 0x001d_7a00, 0, u32::MAX] {
        assert_eq!(
            BtcState::new(code).unwrap_err(),
            Rtw89Error::BtcNotPorted {
                what: "firmware coex layout",
                value: code
            }
        );
    }
    assert!(BtcState::new(0x001d_1d00).is_ok() && BtcState::new(0x001d_79ff).is_ok());
}

#[test]
fn init_writes_the_c_sequence_and_the_h2cs() {
    let mut b = chip(true);
    let mut r = Rec::default();
    let st = inited(&mut b, &mut r);
    let mut want = init_cfg();
    want.push(rd(1, 0x73));
    for f in [
        SET_DRV, SET_DRV, SET_SLOT, SET_MREG, SET_MREG, SET_SLOT, SET_DRV, SET_DRV,
    ] {
        want.push(h2c_mark(f));
    }
    want.extend(ant_wl(true));
    want.push(h2c_mark(SET_POLICY));
    want.extend(preagc_dis());
    want.extend(rpt_cmds());
    want.push(h2c_mark(SET_DRV));
    want.extend(sb(0x4003));
    same(&norm(&b, &r, 0), &want, "btcInit");
    assert!(!st.dm.pta_owner_warn, "0x73 bit 2 clear before init");
    assert_eq!(
        st.events(),
        [BtcEvent::Action {
            reason: rsn::NTFY_INIT,
            action: act::WL_INIT
        }]
    );

    // H2C layouts (fw.c rtw89_fw_h2c_cxdrv_init / _ctrl, coex.c set_slots / monreg / en_rpt /
    // policy)
    let sent = &r.h2c;
    for (_, h, _) in sent {
        assert_eq!((h.cat, h.cls, h.rack), (2, 0x10, false));
        assert_eq!(h.dack, h.func != SET_DRV, "done-ack for _send_fw_cmd only");
    }
    assert_eq!(
        sent[0].2,
        [0, 12, 0, 2, 10, 0, 1, 1, 2, 0, 6, 2, 0, 0],
        "cxinit"
    );
    assert_eq!(sent[1].2, [6, 4, 0, 0, 0, 0], "cxctrl");
    let mut slots = vec![1u8, 18];
    for s in S_DEF {
        slots.extend(slot(s));
    }
    assert_eq!(sent[2].2, slots, "slot table v1");
    let mreg: [u32; 16] = [
        0xda24, 0xda28, 0xda2c, 0xda30, 0xda4c, 0xda10, 0xda20, 0xda34, 0xcef4, 0x8424, 0xd200,
        0xd220, 0x980, 0x4738, 0x4688, 0x4694,
    ];
    let mut m = vec![2u8, 16];
    for (i, o) in mreg.iter().enumerate() {
        m.extend_from_slice(&(if i < 12 { 0u16 } else { 1 }).to_le_bytes());
        m.extend_from_slice(&4u16.to_le_bytes());
        m.extend_from_slice(&o.to_le_bytes());
    }
    assert_eq!(sent[3].2, m, "mon reg table (fcxmreg 2, v1 layout)");
    for (i, j) in [(4, 3), (5, 2), (6, 0), (7, 1)] {
        assert_eq!(sent[i].2, sent[j].2, "the second round repeats the first");
    }
    assert_eq!(
        sent[8].2,
        [tdma_tlv(1, 0), slot_tlv(0, &OFF_BT_SLOT)].concat(),
        "policy at init (OFF_BT)"
    );
    for i in 0..14u32 {
        let mut x = vec![5u8];
        x.extend_from_slice(&i.to_le_bytes());
        x.extend_from_slice(&u32::from(i == 2).to_le_bytes());
        assert_eq!(sent[9 + i as usize].2, x, "report enable {i}");
    }
    let mut role0 = vec![0u8; 66];
    (role0[0], role0[1]) = (1, 64);
    assert_eq!(sent[23].2, role0, "role v2 at init");
    assert_eq!(st.fwinfo.cnt_h2c, 19);
}

#[test]
fn rfk_notifications_and_a_band_switch_inside_one() {
    let mut b = chip(true);
    let mut r = Rec::default();
    let mut st = inited(&mut b, &mut r);
    let h = r.h2c.len();
    let chan = crate::phy::Chan {
        band: Band::B2g,
        ch: 1,
        bw: 20,
    };
    let pm = btc_phymap(Some(&chan), 0, 0).unwrap();
    assert_eq!(pm, 1 << 4, "phy map PHY0 2G");
    let w = |b: &mut RfFake, st: &mut BtcState, r: &mut Rec, typ, state| {
        role::wl_rfk(b, st, r, pm, typ, state).unwrap()
    };
    let n = b.trace.len();
    assert!(w(&mut b, &mut st, &mut r, BTC_WRFKT_DACK, BTC_WRFK_START));
    assert!(w(&mut b, &mut st, &mut r, BTC_WRFKT_DACK, BTC_WRFK_STOP));
    assert!(w(
        &mut b,
        &mut st,
        &mut r,
        BTC_WRFKT_IQK,
        BTC_WRFK_ONESHOT_START
    ));
    assert!(!w(
        &mut b,
        &mut st,
        &mut r,
        BTC_WRFKT_RXDCK,
        BTC_WRFK_ONESHOT_START
    ));
    assert!(
        !w(&mut b, &mut st, &mut r, BTC_WRFKT_DPK, 7),
        "unknown state"
    );
    role::conn_rfk(&mut st, true);
    role::preserve_bt_time(&mut b, &st, 30).unwrap();
    role::conn_rfk(&mut st, false);
    let want = [
        ant_wl(true),
        preagc_dis(),
        sb(0x4803),
        ant_wl(true),
        preagc_dis(),
        sb(0x4003),
    ]
    .concat();
    same(&norm(&b, &r, n), &want, "rfk start/stop");
    let n = b.trace.len();
    w(&mut b, &mut st, &mut r, BTC_WRFKT_IQK, BTC_WRFK_START);
    switch_band(&mut b, &mut st, &mut r, Band::B2g).unwrap();
    w(&mut b, &mut st, &mut r, BTC_WRFKT_IQK, BTC_WRFK_STOP);
    let want = [
        ant_wl(true),
        preagc_dis(),
        sb(0x4803),
        preagc_dis(),
        ant_wl(true),
        preagc_dis(),
        sb(0x4003),
    ]
    .concat();
    same(
        &norm(&b, &r, n),
        &want,
        "band switch inside an RFK: _set_ant returns by wl rfk",
    );
    assert_eq!(r.h2c.len(), h, "no H2C around RFK");
    assert_eq!(
        st.take_events(),
        [
            BtcEvent::Action {
                reason: rsn::NTFY_INIT,
                action: act::WL_INIT
            },
            BtcEvent::Action {
                reason: rsn::NTFY_WL_RFK,
                action: act::BT_OFF
            }
        ]
    );
}

/// The bring-up's device after radio on, as test-btc.mjs's `d` there
fn after_radio_on() -> (RfFake, Rec, BtcState) {
    let mut b = chip(true);
    let mut r = Rec::default();
    let mut st = inited(&mut b, &mut r);
    let n = b.trace.len();
    let h = r.h2c.len();
    radio_on(&mut b, &mut st, &mut r).unwrap();
    same(&norm(&b, &r, n), &init_cfg(), "btcRadioOn");
    assert_eq!(r.h2c.len(), h, "radio on: no H2C");
    assert_eq!(st.dm.tdma_instant_excute, 1, "TDMA instant execute pending");
    (b, r, st)
}

#[test]
fn radio_on_then_a_forced_policy_at_the_band_switch() {
    let (mut b, mut r, mut st) = after_radio_on();
    let n = b.trace.len();
    let h = r.h2c.len();
    switch_band(&mut b, &mut st, &mut r, Band::B2g).unwrap();
    let want = [
        ant_wl(true),
        vec![h2c_mark(SET_POLICY)],
        preagc_dis(),
        sb(0x4003),
    ]
    .concat();
    same(&norm(&b, &r, n), &want, "btcSwitchBand");
    assert_eq!(r.h2c[h].2, full_policy(OFF_BT_SLOT), "forced policy");
    assert_eq!(r.h2c[h].2.len(), 230);
    assert_eq!(st.dm.tdma_instant_excute, 0, "instant execute cleared");
}

#[test]
fn scan_role_packets_and_the_stop() {
    let (mut b, mut r, mut st) = after_radio_on();
    switch_band(&mut b, &mut st, &mut r, Band::B2g).unwrap();
    // scan on channel 6
    let (n, h) = (b.trace.len(), r.h2c.len());
    scan(&mut b, &mut st, &mut r, true, Band::B2g).unwrap();
    scan(&mut b, &mut st, &mut r, false, Band::B2g).unwrap();
    let want = [
        ant_wl(true),
        vec![h2c_mark(SET_POLICY)],
        preagc_dis(),
        ant_wl(true),
        vec![h2c_mark(SET_POLICY)],
        preagc_dis(),
    ]
    .concat();
    same(&norm(&b, &r, n), &want, "scan start/finish");
    assert_eq!(r.h2c[h].2, tdma_tlv(1, 0), "scan start: only the TDMA TLV");
    assert_eq!(r.h2c[h + 1].2, full_policy(OFF_BT_SLOT), "finish: forced");
    assert_eq!((st.ctrl.fw_scan, st.wl.scan_phy_map), (1, 0));

    // add_interface, CONN_START, CONN_END
    let req = RoleReq {
        ch: 6,
        ..RoleReq::default()
    };
    let (n, h) = (b.trace.len(), r.h2c.len());
    role::role(&mut b, &mut st, &mut r, RoleState::Start, &req).unwrap();
    assert_eq!(b.trace.len(), n, "role start writes nothing");
    role::connect(&mut b, &mut st, &mut r, true, &req).unwrap();
    role::connect(&mut b, &mut st, &mut r, false, &req).unwrap();
    let want = [
        ant_wl(true),
        vec![h2c_mark(SET_POLICY)],
        preagc_dis(),
        vec![h2c_mark(SET_DRV)],
        ant_wl(true),
        preagc_dis(),
    ]
    .concat();
    same(&norm(&b, &r, n), &want, "connect");
    let mut role1 = vec![0u8; 66];
    (role1[0], role1[1], role1[3], role1[4]) = (1, 64, 1, 2);
    (role1[6], role1[8], role1[9]) = (1, 1, 6);
    role1[62] = 0x10; // link_mode_chg in the dbcc word
    assert_eq!(r.h2c[h + 1].2, role1, "role v2 after association");
    assert_eq!(st.wl.role.link_mode, wlink::STA);
    assert_eq!(st.wl.role.link_mode_chg, 0, "cleared by the run");
    assert_eq!(st.wl.status.transacting, 0);

    // packets
    let n = b.trace.len();
    role::packet(&mut b, &mut st, &mut r, Packet::Arp).unwrap();
    role::packet(&mut b, &mut st, &mut r, Packet::Icmp).unwrap();
    assert_eq!(b.trace.len(), n, "ARP and ICMP only counted");
    for p in [Packet::Eapol, Packet::EapolEnd, Packet::Dhcp] {
        let n = b.trace.len();
        role::packet(&mut b, &mut st, &mut r, p).unwrap();
        same(
            &norm(&b, &r, n),
            &[ant_wl(true), preagc_dis()].concat(),
            "packet",
        );
    }
    assert_eq!((st.wl.status.dhcp, st.wl.status.four_way), (1, 0));
    let n = b.trace.len();
    role::act1_work(&mut b, &mut st, &mut r).unwrap();
    same(
        &norm(&b, &r, n),
        &[ant_wl(true), preagc_dis()].concat(),
        "act1",
    );
    assert_eq!(st.wl.status.dhcp, 0, "act1 work clears DHCP");

    // radio off and power off (rtw89_core_stop); runs after that return at once
    let (n, h) = (b.trace.len(), r.h2c.len());
    radio_off(&mut b, &mut st, &mut r).unwrap();
    power_off(&mut b, &mut st, &mut r).unwrap();
    // _write_scbd(BTC_WSCB_ALL) contains the RXGAIN bits, so both write the scoreboard at once
    let want = [
        sb(0),
        ant_wl(false),
        vec![h2c_mark(SET_POLICY)],
        preagc_dis(),
        rpt_cmds(),
        vec![h2c_mark(SET_DRV)],
        sb(0),
    ]
    .concat();
    same(&norm(&b, &r, n), &want, "radio off + power off");
    let eq0 = BtcSlot {
        dur: 100,
        cxtbl: 0xffff_ffff,
        cxtype: 1,
    };
    assert_eq!(r.h2c[h].2, full_policy(eq0), "policy OFF_EQ0");
    assert!(
        r.h2c[h + 1..h + 15].iter().all(|x| x.2[5..9] == [0; 4]),
        "all reports off"
    );
    let (n, h) = (b.trace.len(), r.h2c.len());
    scan(&mut b, &mut st, &mut r, true, Band::B2g).unwrap();
    role::packet(&mut b, &mut st, &mut r, Packet::Dhcp).unwrap();
    assert!(
        b.trace.len() == n && r.h2c.len() == h,
        "_run_coex returns for WL rf off"
    );
    assert_eq!(
        st.events().last(),
        Some(&BtcEvent::Action {
            reason: rsn::NTFY_RADIO_STATE,
            action: act::WL_OFF
        })
    );
}

fn rpt_of(typ: u8, c: &[u8], len: usize) -> Vec<u8> {
    let mut b = vec![typ];
    b.extend_from_slice(&(len as u16).to_le_bytes());
    b.extend_from_slice(c);
    b
}
fn ctrl_rpt(en: u32) -> Vec<u8> {
    let mut c = vec![0u8; 80];
    c[0] = 5;
    c[32..36].copy_from_slice(&en.to_le_bytes());
    c
}
fn mreg_rpt(v4688: u32, v980: u32) -> Vec<u8> {
    let mut c = vec![0u8; 84];
    (c[0], c[1]) = (2, 16);
    c[52..56].copy_from_slice(&v980.to_le_bytes());
    c[60..64].copy_from_slice(&v4688.to_le_bytes());
    c
}
fn tdma_rpt(t: &[u8]) -> Vec<u8> {
    [&[3u8, 0, 0, 0][..], t].concat()
}
pub(super) fn c2h_msg(func: u32, payload: Vec<u8>) -> C2h {
    C2h {
        cat: 2,
        cls: 0x12,
        func,
        len: payload.len() as u32 + 8,
        seq: None,
        payload,
        ack: None,
    }
}
const RAW_CTRL: u8 = 0;
const RAW_TDMA: u8 = 1;
const RAW_SLOT: u8 = 2;
const RAW_MREG: u8 = 6;
const RAW_TEST: u8 = 12;

#[test]
fn the_firmwares_reports_change_later_runs() {
    let agc = alloc::rc::Rc::new(core::cell::Cell::new(None::<u32>));
    let mut m = chip(true);
    {
        let agc = agc.clone();
        m.answer_fn(Key::Bb(0x4688), move |regs| {
            agc.get()
                .unwrap_or_else(|| regs.get(BB + 0x4688, crate::bus::Width::W32))
        });
    }
    let mut r = Rec::default();
    let mut st = inited(&mut m, &mut r);
    let rep = |st: &mut BtcState, reps: &[Vec<u8>]| c2h(st, &c2h_msg(0, reps.concat()));
    assert_eq!(st.dm.wl_pre_agc_rb, BTC_PREAGC_NOTFOUND, "no report yet");
    let first = [
        rpt_of(RAW_CTRL, &ctrl_rpt(4), 80),
        rpt_of(RAW_MREG, &mreg_rpt(0x1a00_0000, 0), 84),
    ];
    assert!(!rep(&mut st, &first), "report C2H");
    assert_eq!(
        (
            st.dm.wl_pre_agc_rb,
            st.dm.wl_btg_rx_rb,
            st.fwinfo.rpt_en_map
        ),
        (0, 1, 4),
        "CTRL + MREG: map, pre-AGC off, GNT mux 0"
    );
    let n = m.trace.len();
    scan(&mut m, &mut st, &mut r, true, Band::B2g).unwrap();
    same(
        &norm(&m, &r, n),
        &ant_wl(true),
        "scan after the MREG report: no pre-AGC table",
    );
    let n = m.trace.len();
    switch_band(&mut m, &mut st, &mut r, Band::B2g).unwrap();
    same(
        &norm(&m, &r, n),
        &[ant_wl(true), preagc_dis()].concat(),
        "band switch still writes the pre-AGC table",
    );
    rep(&mut st, &[rpt_of(RAW_MREG, &mreg_rpt(0x8000_0000, 0), 84)]);
    assert_eq!(st.dm.wl_pre_agc_rb, 1, "MREG: pre-AGC on");
    let n = m.trace.len();
    role::packet(&mut m, &mut st, &mut r, Packet::Eapol).unwrap();
    same(
        &norm(&m, &r, n),
        &[ant_wl(true), vec![rd(4, BB + 0x4688)]].concat(),
        "readback differs: BB register read, still off, no table",
    );
    rep(&mut st, &[rpt_of(RAW_MREG, &mreg_rpt(0x8000_0000, 0), 84)]);
    agc.set(Some(0x8012_3456));
    let n = m.trace.len();
    role::act1_work(&mut m, &mut st, &mut r).unwrap();
    same(
        &norm(&m, &r, n),
        &[ant_wl(true), vec![rd(4, BB + 0x4688)], preagc_dis()].concat(),
        "register says on: table written",
    );
    agc.set(None);
    // a report whose length is not the version's stops the walk (_chk_btc_report returns 0)
    st.take_events();
    let bad = [
        rpt_of(RAW_CTRL, &ctrl_rpt(0x7f), 79),
        rpt_of(RAW_MREG, &mreg_rpt(0x1a00_0000, 0), 84),
    ];
    rep(&mut st, &bad);
    assert!(
        st.fwinfo.rpt_en_map == 4 && st.dm.wl_pre_agc_rb != 0,
        "wrong length stops the walk"
    );
    assert_eq!(
        st.take_events(),
        [BtcEvent::ReportRejected {
            typ: BTC_RPT_TYPE_CTRL,
            len: 79,
            want: 80
        }]
    );
    rep(
        &mut st,
        &[
            rpt_of(RAW_TEST, &[0; 4], 4),
            rpt_of(RAW_MREG, &mreg_rpt(0x1a00_0000, 0), 84),
        ],
    );
    assert!(
        st.dm.wl_pre_agc_rb != 0,
        "unknown report type stops the walk"
    );
    rep(
        &mut st,
        &[rpt_of(RAW_MREG, &mreg_rpt(0x1a00_0000, 1 << 17), 84)],
    );
    assert_eq!(
        (st.dm.wl_pre_agc_rb, st.dm.wl_btg_rx_rb),
        (0, 0),
        "MREG alone: GNT mux set"
    );
    // TDMA reports that differ from tdma_now three times force the next policy
    let t1 = rpt_of(RAW_TDMA, &tdma_rpt(&[1, 0, 0, 0, 0, 0, 0, 0]), 12);
    for _ in 0..2 {
        rep(&mut st, core::slice::from_ref(&t1));
    }
    assert!(!st.dm.error_tdma, "below BTC_CHK_HANG_MAX");
    st.take_events();
    rep(&mut st, core::slice::from_ref(&t1));
    assert!(st.dm.error_tdma, "TDMA no-sync after 3 reports");
    assert_eq!(
        st.take_events(),
        [BtcEvent::NoSync {
            what: "TDMA",
            count: 3
        }]
    );
    let (n, h) = (m.trace.len(), r.h2c.len());
    role::packet(&mut m, &mut st, &mut r, Packet::Dhcp).unwrap();
    same(
        &norm(&m, &r, n),
        &[ant_wl(true), vec![h2c_mark(SET_POLICY)]].concat(),
        "no-sync forces the policy",
    );
    assert_eq!(r.h2c[h].2.len(), 230, "forced policy: TDMA + 18 slots");
    assert_eq!(st.dm.tdma_now[7], 1);
    let now = st.dm.tdma_now;
    rep(&mut st, &[rpt_of(RAW_TDMA, &tdma_rpt(&now), 12)]);
    assert!(
        !st.dm.error_tdma && st.dm.cnt_tdma_nonsync == 0,
        "in sync again"
    );
    let mut slots = vec![0u8; 8 + 8 * 18];
    (slots[0], slots[1]) = (1, 18);
    for (i, s) in st.dm.slot_now.iter().enumerate() {
        h2c::put_slot(&mut slots[8 + 8 * i..], s);
    }
    for _ in 0..3 {
        rep(&mut st, &[rpt_of(RAW_SLOT, &slots, 152)]);
    }
    assert!(
        !st.dm.error_slot && st.dm.cnt_slot_nonsync == 0,
        "slot table in sync"
    );
    slots[8] ^= 1;
    for _ in 0..3 {
        rep(&mut st, &[rpt_of(RAW_SLOT, &slots, 152)]);
    }
    assert!(st.dm.error_slot, "slot no-sync after 3 reports");
}

#[test]
fn the_role_layout_keeps_linuxs_port_stride() {
    let mut b = chip(true);
    let mut r = Rec::default();
    let mut st = inited(&mut b, &mut r);
    let rr = &mut st.wl.role;
    let rl = |pid, phy, noa, rf_band, client_ps, bw, role, ch, noa_dur| state::Rlink {
        connected: 1,
        pid,
        phy,
        noa,
        rf_band,
        client_ps,
        bw,
        role,
        ch,
        noa_dur,
        ..Default::default()
    };
    rr.rlink[1] = rl(1, 0, 1, 1, 1, 2, 1, 40, 0x1122_3344);
    rr.rlink[3] = rl(3, 1, 0, 2, 0, 1, 2, 0x33, 0x5566_7788);
    rr.rlink[4] = rl(4, 1, 1, 1, 1, 3, 3, 0x44, 0x99aa_bbcc);
    rr.mrole_type = 0x0a0b_0c0d;
    rr.mrole_noa_duration = 0x0102_0304;
    let mut want = vec![0u8; 66];
    (want[0], want[1]) = (1, 64);
    want[22..30].copy_from_slice(&[0x63, 0x05, 1, 40, 0x44, 0x33, 0x22, 0x11]);
    want[54..62].copy_from_slice(&[0x0d, 0x0c, 0x0b, 0x0a, 0x04, 0x03, 0x02, 0x01]);
    assert_eq!(
        h2c::h2c_cx_role(&mut b, &st, &mut r).unwrap(),
        want,
        "port records 16 bytes apart, mrole over port 3, no port 4"
    );
}

#[test]
fn a_5ghz_association() {
    let mut b = chip(true);
    let mut r = Rec::default();
    let mut st = inited(&mut b, &mut r);
    let req = RoleReq {
        band: Band::B5g,
        ch: 36,
        ..RoleReq::default()
    };
    role::role(&mut b, &mut st, &mut r, RoleState::Start, &req).unwrap();
    role::connect(&mut b, &mut st, &mut r, true, &req).unwrap();
    let h = r.h2c.len();
    role::connect(&mut b, &mut st, &mut r, false, &req).unwrap();
    let role5 = r.h2c[h..]
        .iter()
        .find(|x| x.1.func == SET_DRV && x.2[0] == 1)
        .expect("role H2C");
    assert_eq!(role5.2.len(), 66);
    assert_eq!(
        role5.2[2..10],
        [0, 1, 2, 0, 0x41, 0x00, 1, 36],
        "link STA, band 1, ch 36"
    );
    assert_eq!((st.wl.rf_band_map[0], st.wl.is_5g_hi_ch), (2, 0));
}

#[test]
fn chip_ops_not_reached_on_this_path() {
    let mut t = chip(true);
    let mut st = BtcState::new(FW_CODE).unwrap();
    hw::btc_set_wl_txpwr_ctrl(&mut t, 0xffff_ffff).unwrap();
    hw::btc_set_wl_txpwr_ctrl(&mut t, 0xffff_0010).unwrap();
    hw::btc_set_wl_rx_gain(&mut t, &mut st, 2).unwrap();
    hw::btc_set_wl_rx_gain(&mut t, &mut st, 0).unwrap();
    hw::btc_wl_s1_standby(&mut t, true).unwrap();
    hw::btc_wl_s1_standby(&mut t, false).unwrap();
    let lna2 = |odd: u32| {
        let mut v = rf(1, RR_LUTWE, 0x1000);
        for (a, x) in [(0, 0x15), (1, odd), (2, 0x15), (3, odd)] {
            v.extend(rf(1, RR_LUTWA, a));
            v.extend(rf(1, RR_LUTWD0, x));
        }
        v.extend(rf(1, RR_LUTWE, 0));
        v
    };
    let s1 = |v: u32| {
        [
            rf(1, RR_LUTWE, 0x80000),
            rf(1, RR_LUTWA, 1),
            rf(1, RR_LUTWD1, 0x31),
            rf(1, RR_LUTWD0, v),
            rf(1, RR_LUTWE, 0),
        ]
        .concat()
    };
    let want = [
        vec![
            rmw(4, 0xD200, 0x3ff, 0),
            rmw(4, 0xD220, 0xffa, 0),
            rmw(4, 0xD200, 0x3ff, 0x210),
            rmw(4, 0xD220, 0xffa, 0),
        ],
        preagc_dis(),
        lna2(5),
        preagc_dis(),
        lna2(0x17),
        s1(0x179),
        s1(0x20),
    ]
    .concat();
    same(&t.lines(), &want, "txpwr ctrl, rx gain 2 and 0, S1 standby");
    assert_eq!(
        (
            hw::btc_get_bt_rssi(-10),
            hw::btc_get_bt_rssi(0),
            hw::btc_get_bt_rssi(-120)
        ),
        (96, 100, 0),
        "bt rssi clamp"
    );
}

#[test]
fn guards_cmac_off_pta_owner_bt_on() {
    // CMAC off: no PLT write, logged
    let mut c = chip(false);
    let mut r = Rec::default();
    let mut st = inited(&mut c, &mut r);
    assert!(
        !c.lines().iter().any(|l| l.contains("0x0000C67C")),
        "no PLT write"
    );
    assert!(st.events().contains(&BtcEvent::PltSkipped {
        reg: R_AX_CMAC_FUNC_EN,
        value: 0
    }));
    assert!(
        c2h(&mut st, &c2h_msg(2, vec![2, 0, 0, 0])),
        "BT scoreboard says on"
    );
    assert_eq!(st.bt_reports().len(), 1);
    assert_eq!(
        st.events().last(),
        Some(&BtcEvent::BtScoreboard { bid: 0, scbd: 2 })
    );
    // 0x73 bit 2 already set: the PTA owner warning
    let mut w = chip(true);
    w.answer(Key::Mac(0x73), 0x04);
    let st = inited(&mut w, &mut Rec::default());
    assert!(st.dm.pta_owner_warn);
    assert!(st.events().contains(&BtcEvent::PtaOwnerWarn {
        reg: 0x73,
        value: 4
    }));
    // BT on stops the run, with the scoreboard
    let mut b = chip(true);
    let mut st = BtcState::new(FW_CODE).unwrap();
    st.bt0.scbd_c2h = 1 << 1;
    assert_eq!(
        init(&mut b, &mut st, &mut Rec::default(), RFE1, BTC_MODE_NORMAL).unwrap_err(),
        Rtw89Error::BtcNotPorted {
            what: "BT re-enable (BT scoreboard)",
            value: 2
        }
    );
    // other modes and the dedicated antenna are refused with their value
    for (mode, rfe, what) in [
        (BTC_MODE_BT, 1, "coex mode"),
        (BTC_MODE_WLOFF, 1, "coex mode"),
        (BTC_MODE_COTX, 1, "coex mode"),
        (BTC_MODE_NORMAL, 2, "dedicated antenna (rfe_type even)"),
    ] {
        let mut b = chip(true);
        let mut st = BtcState::new(FW_CODE).unwrap();
        let e = init(
            &mut b,
            &mut st,
            &mut Rec::default(),
            ChipFacts { rfe, cv: 1 },
            mode,
        );
        let value = if mode == BTC_MODE_NORMAL { rfe } else { mode };
        assert_eq!(e.unwrap_err(), Rtw89Error::BtcNotPorted { what, value });
    }
}

#[test]
fn h2cs_reach_ch12_with_linuxs_header() {
    let mut mem = MemBus::new();
    mem.forced.insert(R_AX_CMAC_FUNC_EN, B_AX_CMAC_EN);
    let mut b = HookBus::new(mem);
    let mut rings = crate::pci::pci_alloc(&mut b, &crate::pci::RingConfig::js()).unwrap();
    let mut fws = FwState::new();
    let mut st = BtcState::new(FW_CODE).unwrap();
    let mut phy = Phy::new(1);
    assert_eq!(
        core_start_init(&mut b, &mut st, &mut rings, &mut fws, &phy).unwrap_err(),
        Rtw89Error::Invalid {
            what: "btc init before the efuse",
            value: 0
        }
    );
    phy.efuse = Some(crate::phy::efuse::Efuse {
        rfe_type: 1,
        ..Default::default()
    });
    let from = b.writes.len();
    core_start_init(&mut b, &mut st, &mut rings, &mut fws, &phy).unwrap();
    let funcs: Vec<u32> = b.writes[from..]
        .iter()
        .map(|(_, d)| {
            let w0 = u32::from_le_bytes([d[24], d[25], d[26], d[27]]);
            let w1 = u32::from_le_bytes([d[28], d[29], d[30], d[31]]);
            assert_eq!(field(w0, H2C_HDR_CAT), 2, "OUTSRC");
            assert_eq!(field(w0, H2C_HDR_CLASS), 0x10, "BTFC_SET");
            assert_eq!(
                field(w1, H2C_HDR_TOTAL_LEN) as usize,
                d.len() - 24,
                "header + payload"
            );
            field(w0, H2C_HDR_FUNC)
        })
        .collect();
    assert_eq!(funcs.len(), 24, "the init's 24 H2Cs on CH12");
    assert_eq!(funcs[..8], [5, 5, 1, 2, 2, 1, 5, 5]);
    assert_eq!(funcs[8], SET_POLICY);
    assert!(funcs[9..23].iter().all(|&f| f == SET_RPT));
    assert_eq!(funcs[23], SET_DRV);
    assert_eq!(fws.h2c_seq, 24);
    // through rfk1's MAC side the same bytes go out
    let mut mac = crate::macinit::MacState::new();
    let mut link = MacLink {
        mac: &mut mac,
        fw: &mut fws,
        rings: Some(&mut rings),
    };
    let from = b.writes.len();
    h2c::h2c_cx_ctrl(&mut b, &st, &mut link).unwrap();
    assert_eq!(b.writes[from].1[32..], [6, 4, 0, 0, 0, 0]);
}

#[test]
fn wl_sta_tracks_rssi_and_traffic() {
    let mut b = chip(true);
    let mut r = Rec::default();
    let mut st = inited(&mut b, &mut r);
    let req = RoleReq::default();
    role::role(&mut b, &mut st, &mut r, RoleState::Start, &req).unwrap();
    role::connect(&mut b, &mut st, &mut r, true, &req).unwrap();
    role::connect(&mut b, &mut st, &mut r, false, &req).unwrap();
    let link = |avg_rssi, tx, rx, rate| WlStaLink {
        port: 0,
        avg_rssi,
        stats: state::Traffic {
            tx_tfc_lv: tx,
            rx_tfc_lv: rx,
            ..Default::default()
        },
        tx_hw_rate: 0,
        rx_hw_rate: rate,
    };
    // RSSI 45 (avg 90 >> RSSI_FACTOR): below 70, 60, 50, above 40 -> level 3. Idle: the direction
    // becomes DL, a station change, so coex runs once
    wlsta::wl_sta(&mut b, &mut st, &mut r, Some(&link(90, 0, 0, 0))).unwrap();
    assert_eq!(st.wl.rssi_level, 3);
    assert_eq!(st.dm.run_reason, rsn::NTFY_WL_STA);
    assert_eq!(st.dm.trx_info.wl_rssi, 3);
    // the same again: nothing to tell
    let (n, h) = (b.trace.len(), r.h2c.len());
    wlsta::wl_sta(&mut b, &mut st, &mut r, Some(&link(90, 0, 0, 0))).unwrap();
    assert!(b.trace.len() == n && r.h2c.len() == h, "nothing changed");
    // busy RX with a rate: the role H2C (traffic change), a coex run (station change)
    st.dm.run_reason = rsn::NONE;
    wlsta::wl_sta(&mut b, &mut st, &mut r, Some(&link(90, 0, 2, 0x10b))).unwrap();
    assert_eq!(r.h2c.len(), h + 1);
    assert_eq!(st.dm.run_reason, rsn::NTFY_WL_STA);
    assert_eq!(
        (st.wl.status.busy, st.wl.status.traffic_dir),
        (1, 1 << RTW89_TFC_DL)
    );
    // the next call marks WLBUSY in the scoreboard (with the old busy); it waits for a run.
    // The role H2C goes again: the "last" rate reads back 0, as in Linux
    let n = b.trace.len();
    wlsta::wl_sta(&mut b, &mut st, &mut r, Some(&link(90, 0, 2, 0x10b))).unwrap();
    assert_eq!(st.wl.scbd & crate::tables::btc::wscb::WLBUSY, 0x80);
    assert_eq!(st.wl.scbd_chg, 1);
    assert_eq!(b.trace.len(), n, "no register");
    assert_eq!(r.h2c.len(), h + 2);
}
