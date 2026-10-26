//! chan without node: the channel values, the table loaders and readers on hand-made elements,
//! the refusals before any access, the good switch's neighbours and events, and a failure
//! injected at every op of a good switch (the TX power and the help's leave still run, the
//! scheduler's TX enables come back). The JS module itself is the oracle in tests/chan_js.rs,
//! the golden trace in tests/fake_golden.rs.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use super::fw::{TxpwrConf, load_regd_map_from, load_rfe_data_from};
use super::txpwr::{cal_txpwr_ref, channel_to_idx, mac_txpwr_w32, read_txpwr_limit};
use super::*;
use crate::bus::{BusResult, MemWidth, Width, prep};
use crate::phy::efuse::Efuse;
use crate::phy::io::BB;
use crate::rfk1::fake::{Key, RfFake};

fn conf(key: &'static str, ent_sz: u8, ents: &[&[u8]]) -> TxpwrConf {
    let mut data = Vec::new();
    for e in ents {
        data.extend_from_slice(e);
    }
    TxpwrConf {
        key,
        id: 0,
        rfe: 0,
        ent_sz,
        num_ents: ents.len() as u32,
        data,
    }
}

/// Hand-made configurations: CCK by rate 80 (RF units) on 2.4 GHz, OFDM limits on 2.4 GHz
/// channel 1 (ETSI 58) and channel 2 (WW 40 only), a 5 GHz limit of 127 on channel 36
fn confs() -> Vec<TxpwrConf> {
    vec![
        conf(
            "byrate",
            11,
            &[&[0, 0, RTW89_RS_CCK as u8, 0, 4, 80, 80, 80, 80, 0, 0]],
        ),
        conf(
            "lmt2g",
            7,
            &[
                &[0, 0, 1, 0, RTW89_ETSI as u8, 0, 58],
                &[0, 0, 1, 0, 0, 1, 40],
            ],
        ),
        conf("lmt5g", 7, &[&[0, 0, 1, 0, RTW89_ETSI as u8, 0, 127]]),
        conf("lmtRu2g", 5, &[&[0, 0, RTW89_ETSI as u8, 0, 32]]),
        conf("lmtRu5g", 5, &[]),
        conf("txShapeLmt", 5, &[&[0, 0, RTW89_ETSI as u8, 1, 0]]),
        conf("txShapeLmtRu", 4, &[]),
    ]
}

fn regd(a: &[u8; 2], r: [u32; 3]) -> Regd {
    Regd {
        alpha2: *a,
        txpwr_regd: r,
        fmap: 0,
    }
}

/// A channel state as after `load` and phy_dm_init (cfg_txrx_path)
fn ready() -> ChanState {
    let mut st = ChanState::new();
    st.tables = Some(load_rfe_data_from(&confs()).unwrap());
    st.regd_map = Some(vec![
        regd(b"DE", [RTW89_ETSI, RTW89_ETSI, RTW89_NA]),
        regd(b"GB", [RTW89_UK, RTW89_UK, RTW89_UK]),
        regd(b"TV", [RTW89_ETSI, RTW89_NA, RTW89_NA]),
    ]);
    regd_apply(&mut st, DEFAULT_COUNTRY).unwrap();
    st.dm_init_done = true;
    st.take_events();
    st
}

fn phy() -> Phy {
    let mut p = Phy::new(1);
    let mut e = Efuse::default();
    e.gain.offset_valid = true;
    e.gain.offset_base = [-12, 0];
    e.gain.rssi_base = [16, 0];
    p.efuse = Some(e);
    p
}

fn chip() -> RfFake {
    let mut b = RfFake::new();
    b.answer(
        Key::Mac(R_AX_CMAC_FUNC_EN),
        B_AX_CMAC_EN | B_AX_CMAC_TXEN | B_AX_CMAC_RXEN,
    )
    .answer(Key::Bb(R_CHBW_MOD_V1), prep(B_ANT_RX_SEG0, 3));
    b.poke(R_AX_CTN_TXEN, Width::W32, 0x00ff);
    b
}

/// Neighbours that note their calls; stop/resume on the register (two ops for the stop)
#[derive(Default)]
struct Calls(Vec<String>);

impl<B: Bus + ?Sized> ChanHooks<B> for Calls {
    fn stop_sch_tx(&mut self, bus: &mut B) -> Rtw89Result<u16> {
        self.0.push("stop".into());
        let tx_en = bus.r16(R_AX_CTN_TXEN)?;
        bus.w16(R_AX_CTN_TXEN, 0)?;
        Ok(tx_en)
    }
    fn resume_sch_tx(&mut self, bus: &mut B, tx_en: u16) -> Rtw89Result<()> {
        self.0.push(format!("resume {tx_en:#x}"));
        bus.w16(R_AX_CTN_TXEN, tx_en.into())?;
        Ok(())
    }
    fn set_channel_rf(&mut self, _: &mut B, chan: &Chan) -> Rtw89Result<()> {
        self.0.push(format!("rf {}", chan.ch));
        Ok(())
    }
    fn btc_switch_band(&mut self, _: &mut B, band: Band) -> Rtw89Result<()> {
        self.0.push(format!("btc {band:?}"));
        Ok(())
    }
    fn rfk_band_changed(&mut self, _: &mut B, _: &mut Phy, chan: &Chan) -> Rtw89Result<()> {
        self.0.push(format!("band changed {}", chan.ch));
        Ok(())
    }
    fn rfk_channel(&mut self, _: &mut B, _: &mut Phy, chan: &Chan) -> Rtw89Result<()> {
        self.0.push(format!("rfk channel {}", chan.ch));
        Ok(())
    }
}

fn req(band: Band, ch: u32) -> ChanReq {
    ChanReq { band, ch, bw: 20 }
}

#[test]
fn channel_values() {
    let c = chan_create(Band::B2g, 1);
    assert_eq!(
        (c.freq, c.subband_type, c.tx_comp_band, c.rfsi_band),
        (
            2412,
            RTW89_CH_2G,
            RTW89_TX_COMP_BAND_2GHZ,
            RFSI_CTRL_BAND_2GHZ
        )
    );
    assert_eq!(c.pri_ch_idx, RTW89_SC_DONT_CARE);
    assert_eq!(chan_create(Band::B2g, 14).freq, 2484);
    let c = chan_create(Band::B5g, 36);
    assert_eq!(
        (c.freq, c.subband_type, c.tx_comp_band, c.rfsi_band),
        (
            5180,
            RTW89_CH_5G_BAND_1,
            RTW89_TX_COMP_BAND_5GHZ_L,
            RFSI_CTRL_BAND_5_6GHZ
        )
    );
    assert_eq!(chan_create(Band::B5g, 100).subband_type, RTW89_CH_5G_BAND_3);
    let c = chan_create(Band::B5g, 149);
    assert_eq!(
        (c.subband_type, c.tx_comp_band),
        (RTW89_CH_5G_BAND_4, RTW89_TX_COMP_BAND_5GHZ_H)
    );
    assert_eq!(RtwChan::ZERO.phy_chan(), None);
    assert_eq!(
        c.phy_chan(),
        Some(Chan {
            band: Band::B5g,
            ch: 149,
            bw: 20
        })
    );
    // rtw89_encode_chan_idx (test-chan.mjs's expectations) and the SCO mapping
    for (ch, idx) in [
        (36, 0x20),
        (44, 0x24),
        (100, 0x30),
        (140, 0x44),
        (149, 0x50),
        (165, 0x58),
    ] {
        assert_eq!(bb::encode_chan_idx(ch, RTW89_BAND_5G), idx, "ch {ch}");
    }
    assert_eq!(bb::encode_chan_idx(13, RTW89_BAND_2G), 13);
    assert_eq!(bb::encode_chan_idx(20, RTW89_BAND_5G), 0);
    for (ch, sco) in [
        (1, 109),
        (6, 108),
        (13, 106),
        (36, 51),
        (44, 50),
        (100, 48),
        (140, 46),
        (149, 46),
        (165, 45),
        (0, 0),
        (200, 0),
    ] {
        assert_eq!(bb::sco_mapping(ch), sco, "ch {ch}");
    }
    for (ch, idx) in [
        (1, 0),
        (14, 13),
        (36, 0),
        (64, 14),
        (100, 15),
        (144, 37),
        (149, 38),
        (177, 52),
        (0, 0),
        (15, 0),
        (255, 0),
    ] {
        assert_eq!(channel_to_idx(ch), idx, "ch {ch}");
    }
    // rtw8852bx_bb_cal_txpwr_ref(0, 0): tssi_ofst_cw 172, pwr_cw 312 (test-chan.mjs)
    assert_eq!(cal_txpwr_ref(0, 0), 0x02B2_7000);
    assert_ne!(cal_txpwr_ref(0, 8), cal_txpwr_ref(0, 0));
    let _ = cal_txpwr_ref(0, -100_000); // the s16 wraps, nothing panics
    let _ = cal_txpwr_ref(i32::MAX, i32::MIN);
    assert_eq!(chan_create(Band::B5g, u32::MAX).freq, u32::MAX);
}

#[test]
fn loaders_take_what_linux_takes() {
    let t = load_rfe_data_from(&confs()).unwrap();
    let h = t.byr_head(RTW89_BAND_2G, 0).unwrap();
    assert_eq!(h.cck, [80; 4]);
    assert!(t.byr_head(3, 0).is_none() && t.byr_head(0, 5).is_none());
    assert!(!t.has_da);
    // invalid by-rate entries are skipped: band 3, bw 5, shift past the section, nss 4 for MCS
    let bad: [&[u8]; 4] = [
        &[3, 0, 0, 0, 1, 1, 0, 0, 0, 0, 0],
        &[0, 0, 0, 0, 1, 1, 0, 0, 0, 5, 0],
        &[0, 0, 0, 3, 2, 1, 1, 0, 0, 0, 0],
        &[0, 4, RTW89_RS_MCS as u8, 0, 1, 1, 0, 0, 0, 0, 0],
    ];
    let mut c = confs();
    c[0] = conf("byrate", 11, &bad);
    let t2 = load_rfe_data_from(&c).unwrap();
    assert!(t2.byr.iter().all(|h| *h == ByrHead::default()));
    // a 9-byte entry (no bw/ofdma) as this firmware's rfe 0 element has them
    c[0] = conf(
        "byrate",
        9,
        &[&[1, 0, RTW89_RS_OFDM as u8, 2, 2, 7, 9, 0, 0]],
    );
    let t2 = load_rfe_data_from(&c).unwrap();
    assert_eq!(t2.byr_head(1, 0).unwrap().ofdm[2..4], [7, 9]);
    // a longer entry is taken only when its extension is zero
    c[1] = conf(
        "lmt2g",
        8,
        &[&[0, 0, 1, 0, 1, 0, 20, 0], &[0, 0, 1, 0, 1, 1, 30, 1]],
    );
    let t2 = load_rfe_data_from(&c).unwrap();
    assert_eq!(t2.lmt_2g.get(&[0, 0, 1, 0, 1, 0]), Some(20));
    assert_eq!(t2.lmt_2g.get(&[0, 0, 1, 0, 1, 1]), Some(0));
    // out of range indexes are ignored, never written
    c[1] = conf(
        "lmt2g",
        7,
        &[
            &[2, 0, 0, 0, 0, 0, 5],
            &[0, 0, 0, 0, 16, 0, 5],
            &[0, 0, 0, 0, 0, 14, 5],
        ],
    );
    let t2 = load_rfe_data_from(&c).unwrap();
    assert!(t2.lmt_2g.data().iter().all(|&v| v == 0));
    // entries past the element's content: FwFile with the count
    let mut short = conf("lmtRu5g", 5, &[&[0, 0, 0, 0, 1]]);
    short.num_ents = 1000;
    c[4] = short;
    assert_eq!(
        load_rfe_data_from(&c),
        Err(Rtw89Error::FwFile {
            what: "txpwr element entries past its content",
            value: 1000
        })
    );
    // a needed element missing: FwFile with its element id (TX_SHAPE_LMT_RU = 17)
    let mut c = confs();
    c.retain(|x| x.key != "txShapeLmtRu");
    assert_eq!(
        load_rfe_data_from(&c),
        Err(Rtw89Error::FwFile {
            what: "firmware lacks a TX power element",
            value: 17
        })
    );
}

fn regd_elm(ent_sz: u8, n: u32, ents: &[u8]) -> Vec<u8> {
    let mut p = vec![0, 0, 0, ent_sz];
    p.extend_from_slice(&n.to_le_bytes());
    p.extend_from_slice(ents);
    p
}

#[test]
fn regd_elements_as_linux() {
    let e1 = regd_elm(9, 1, &[b'D', b'E', 1, 1, 4, 2, 0, 0, 0]);
    // short entries (5 bytes): fmap 0; a rule past RTW89_REGD_NUM becomes NA
    let e2 = regd_elm(5, 2, &[b'A', b'B', 1, 2, 3, b'C', b'D', 200, 1, 1]);
    let m = load_regd_map_from([e1.as_slice(), e2.as_slice()]).unwrap();
    assert_eq!(
        m.map.as_deref(),
        Some([regd(b"AB", [1, 2, 3]), regd(b"CD", [RTW89_NA, 1, 1])].as_slice()),
        "the last element wins"
    );
    let e3 = regd_elm(3, 1, b"EFG");
    assert_eq!(
        load_regd_map_from([e3.as_slice()]).unwrap().map.unwrap()[0],
        regd(b"EF", [RTW89_NA, RTW89_NA, RTW89_NA]),
        "a short entry keeps the defaults"
    );
    let m = load_regd_map_from([e1.as_slice()]).unwrap();
    assert_eq!(m.map.unwrap()[0].fmap, 2);
    // more than 255 countries: ignored (the earlier element stays)
    let big = regd_elm(9, 256, &[]);
    let m = load_regd_map_from([e1.as_slice(), big.as_slice()]).unwrap();
    assert_eq!(m.ignored, [256]);
    assert_eq!(m.map.unwrap().len(), 1);
    // entries past the content
    let cut = regd_elm(9, 3, &[b'D', b'E', 1, 1, 4, 2, 0, 0, 0]);
    assert!(matches!(
        load_regd_map_from([cut.as_slice()]),
        Err(Rtw89Error::FwFile { value: 3, .. })
    ));
    assert_eq!(
        load_regd_map_from([[0u8; 3].as_slice()]).unwrap().map,
        Some(vec![])
    );
    assert_eq!(load_regd_map_from([]).unwrap().map, None);
    // a country the map does not have is worldwide
    let mut st = ready();
    let r = regd_apply(&mut st, *b"XX").unwrap();
    assert_eq!(r, Regd::WW);
    assert_eq!(
        st.take_events(),
        [ChanEvent::RegdSet {
            alpha2: *b"00",
            txpwr_regd: [RTW89_WW; 3]
        }]
    );
    let mut empty = ChanState::new();
    assert!(matches!(
        regd_apply(&mut empty, *b"DE"),
        Err(Rtw89Error::Invalid { .. })
    ));
}

#[test]
fn limits_read_as_phy_c() {
    let st = ready();
    let t = st.tables.as_ref().unwrap();
    let de = st.regd.unwrap();
    let ofdm = |r: &Regd, band, ch| read_txpwr_limit(t, r, band, 0, 0, 1, 0, ch);
    assert_eq!(
        ofdm(&de, RTW89_BAND_2G, 1),
        29,
        "58 RF units -> 29 MAC units"
    );
    assert_eq!(
        ofdm(&de, RTW89_BAND_2G, 2),
        20,
        "ETSI 0 falls back to WW 40"
    );
    assert_eq!(ofdm(&de, RTW89_BAND_2G, 3), 0);
    assert_eq!(
        ofdm(&de, RTW89_BAND_5G, 36),
        63,
        "127 >> 1 capped at 63 (TPE outside 6 GHz)"
    );
    let gb = regd(b"GB", [RTW89_UK; 3]);
    assert_eq!(ofdm(&gb, RTW89_BAND_2G, 1), 29, "UK follows ETSI");
    let tv = regd(b"TV", [RTW89_ETSI, RTW89_NA, RTW89_NA]);
    assert_eq!(ofdm(&tv, RTW89_BAND_5G, 36), 0, "NA: WW has nothing here");
    // a bandwidth past the table and a wild regulation read 0 (or the WW value)
    assert_eq!(read_txpwr_limit(t, &de, RTW89_BAND_2G, 2, 0, 1, 0, 1), 0);
    let wild = regd(b"ZZ", [200, 200, 200]);
    assert_eq!(ofdm(&wild, RTW89_BAND_2G, 2), 20);
    assert_eq!(
        txpwr::read_txpwr_limit_ru(t, &de, RTW89_BAND_2G, 0, 0, 1),
        16
    );
    assert_eq!(txpwr::tx_shape_idx(t, &de, RTW89_BAND_2G, RTW89_RS_CCK), 1);
}

#[test]
fn channel_flags_of_de_and_the_world() {
    let st = ready();
    let f = |b, ch| st.channel_flags(b, ch);
    assert!(f(Band::B2g, 13).allowed && !f(Band::B2g, 14).allowed);
    assert!(f(Band::B5g, 52).radar && !f(Band::B5g, 36).no_ir);
    assert!(!f(Band::B5g, 38).allowed, "off the 20 MHz grid");
    assert!(f(Band::B5g, 165).allowed && !f(Band::B5g, 169).allowed);
    assert!(!f(Band::B6g, 1).allowed);
    let mut w = ready();
    regd_apply(&mut w, *b"US").unwrap();
    assert!(w.channel_flags(Band::B2g, 14).no_ir && w.channel_flags(Band::B2g, 14).allowed);
    assert!(w.channel_flags(Band::B5g, 144).radar);
    // before `load` the country is DE
    assert!(!ChanState::new().channel_flags(Band::B2g, 14).allowed);
}

type Setup = fn(&mut ChanState, &mut Phy);

#[test]
fn refusals_come_before_any_access() {
    let cases: [(&str, ChanReq, Setup); 7] = [
        (
            "40 MHz",
            ChanReq {
                band: Band::B5g,
                ch: 36,
                bw: 40,
            },
            |_, _| {},
        ),
        ("6 GHz", req(Band::B6g, 37), |_, _| {}),
        ("no tables", req(Band::B2g, 1), |s, _| s.tables = None),
        ("no country", req(Band::B2g, 1), |s, _| s.regd = None),
        ("channel 14 in DE", req(Band::B2g, 14), |_, _| {}),
        ("before phy_dm_init", req(Band::B2g, 1), |s, _| {
            s.dm_init_done = false
        }),
        ("no efuse", req(Band::B2g, 1), |_, p| p.efuse = None),
    ];
    for (what, r, f) in cases {
        let mut b = chip();
        let mut st = ready();
        let mut p = phy();
        f(&mut st, &mut p);
        let mut h = Calls::default();
        let e = set_channel(&mut b, &mut p, &mut st, r, &mut h, false);
        assert!(
            matches!(e, Err(Rtw89Error::Invalid { .. })),
            "{what}: {e:?}"
        );
        assert!(b.trace.is_empty() && h.0.is_empty(), "{what}: no access");
        assert_eq!(st.chan, RtwChan::ZERO, "{what}: state untouched");
    }
    // cfg_txrx_path and power_trim without an efuse
    let mut b = chip();
    let mut st = ready();
    let p = Phy::new(1);
    assert!(cfg_txrx_path(&mut b, &p, &mut st).is_err());
    assert!(power_trim(&mut b, &p).is_err());
    assert!(b.trace.is_empty());
}

#[test]
fn good_switches_call_the_neighbours_in_linux_order() {
    let mut b = chip();
    let mut st = ready();
    let mut p = phy();
    let mut h = Calls::default();
    set_channel(&mut b, &mut p, &mut st, req(Band::B2g, 1), &mut h, false).unwrap();
    assert_eq!(
        h.0,
        ["stop", "rf 1", "resume 0xff", "btc B2g", "band changed 1"]
    );
    assert!(st.entity_active && !st.band_changed);
    assert_eq!(b.regs.get(R_AX_CTN_TXEN, Width::W16), 0xff);
    h.0.clear();
    set_channel(&mut b, &mut p, &mut st, req(Band::B2g, 6), &mut h, true).unwrap();
    assert_eq!(
        h.0,
        ["stop", "rf 6", "resume 0xff", "rfk channel 6"],
        "same band"
    );
    h.0.clear();
    set_channel(&mut b, &mut p, &mut st, req(Band::B5g, 36), &mut h, false).unwrap();
    assert_eq!(h.0[3..], ["btc B5g", "band changed 36"]);
    assert_eq!(
        (st.prev_ch, st.prev_band, st.prev_band_type, st.band_changed),
        (6, Band::B2g, RTW89_BAND_2G, true)
    );
    let sw = |band, ch, prev_ch, band_changed| ChanEvent::Switched {
        band,
        ch,
        prev_ch,
        band_changed,
    };
    assert_eq!(
        st.take_events(),
        [
            sw(Band::B2g, 1, 0, false),
            sw(Band::B2g, 6, 1, false),
            sw(Band::B5g, 36, 6, true)
        ]
    );
    // the TX power again for the current channel, then nothing after a power-off
    let n = b.trace.len();
    set_txpwr(&mut b, &mut st).unwrap();
    assert!(b.trace.len() > n);
    power_off(&mut st);
    let n = b.trace.len();
    set_txpwr(&mut b, &mut st).unwrap();
    regd_set(&mut b, &mut st, *b"GB").unwrap();
    assert_eq!(b.trace.len(), n, "inactive: no TX power");
    assert!(set_channel(&mut b, &mut p, &mut st, req(Band::B2g, 1), &mut h, false).is_err());
}

/// `Calls` with one neighbour failing
struct OneFails(Calls, &'static str);

impl<B: Bus + ?Sized> ChanHooks<B> for OneFails {
    fn stop_sch_tx(&mut self, bus: &mut B) -> Rtw89Result<u16> {
        self.0.stop_sch_tx(bus)
    }
    fn resume_sch_tx(&mut self, bus: &mut B, v: u16) -> Rtw89Result<()> {
        self.0.resume_sch_tx(bus, v)
    }
    fn set_channel_rf(&mut self, bus: &mut B, c: &Chan) -> Rtw89Result<()> {
        if self.1 == "rf" {
            return Err(Rtw89Error::Bus(BusError::PollTimeout {
                addr: BB + R_SWSI_V1,
                width: Width::W32,
                mask: 3,
                want: 0,
                last: 3,
            }));
        }
        self.0.set_channel_rf(bus, c)
    }
    fn btc_switch_band(&mut self, bus: &mut B, b: Band) -> Rtw89Result<()> {
        if self.1 == "btc" {
            return Err(Rtw89Error::Invalid {
                what: "btc",
                value: 0,
            });
        }
        self.0.btc_switch_band(bus, b)
    }
    fn rfk_band_changed(&mut self, bus: &mut B, p: &mut Phy, c: &Chan) -> Rtw89Result<()> {
        if self.1 == "tssi" {
            return Err(Rtw89Error::Invalid {
                what: "tssi",
                value: 0,
            });
        }
        self.0.rfk_band_changed(bus, p, c)
    }
    fn rfk_channel(&mut self, bus: &mut B, p: &mut Phy, c: &Chan) -> Rtw89Result<()> {
        self.0.rfk_channel(bus, p, c)
    }
}

// The entity was active (2.4 GHz): a failed band change on the way to 5 GHz must still make
// the next 5 GHz switch run btc and the TSSI scan, although st.chan is 5 GHz already
#[test]
fn a_failing_band_change_of_an_active_entity_is_redone() {
    for which in ["btc", "tssi"] {
        let mut b = chip();
        let mut st = ready();
        let mut p = phy();
        set_channel(
            &mut b,
            &mut p,
            &mut st,
            req(Band::B2g, 1),
            &mut Calls::default(),
            false,
        )
        .unwrap();
        assert!(st.entity_active);
        let mut h = OneFails(Calls::default(), which);
        assert!(set_channel(&mut b, &mut p, &mut st, req(Band::B5g, 36), &mut h, false).is_err());
        assert!(!st.entity_active, "{which}");
        let mut h = Calls::default();
        set_channel(&mut b, &mut p, &mut st, req(Band::B5g, 40), &mut h, false).unwrap();
        assert_eq!(h.0[3..], ["btc B5g", "band changed 40"], "{which}");
        assert!(st.entity_active && !st.band_changed, "{which}");
    }
}

#[test]
fn a_failing_band_change_leaves_the_entity_inactive() {
    let mut b = chip();
    let mut st = ready();
    let mut p = phy();
    let mut h = OneFails(Calls::default(), "btc");
    assert!(set_channel(&mut b, &mut p, &mut st, req(Band::B2g, 1), &mut h, false).is_err());
    assert!(!st.entity_active, "the next switch redoes the band change");
    assert!(matches!(
        st.take_events().last(),
        Some(ChanEvent::Aborted {
            entry: "set_channel band change",
            ..
        })
    ));
    let mut h = Calls::default();
    set_channel(&mut b, &mut p, &mut st, req(Band::B2g, 6), &mut h, false).unwrap();
    assert!(h.0.contains(&"btc B2g".to_string()));
}

type Rule = Box<dyn FnMut(u64, u32) -> Option<BusError>>;

/// RfFake with an error injected where `rule` says (op number from 1, address); after a `Gone`
/// every access fails and is counted
struct Faulty {
    b: RfFake,
    n: u64,
    rule: Rule,
    failed: Vec<u32>,
    after_gone: u64,
    gone: bool,
}

impl Faulty {
    fn new(b: RfFake, rule: Rule) -> Faulty {
        Faulty {
            b,
            n: 0,
            rule,
            failed: Vec::new(),
            after_gone: 0,
            gone: false,
        }
    }
    fn hit(&mut self, addr: u32) -> BusResult<()> {
        self.n += 1;
        if self.gone {
            self.after_gone += 1;
            return Err(BusError::Gone);
        }
        if let Some(e) = (self.rule)(self.n, addr) {
            self.failed.push(addr);
            self.gone = e == BusError::Gone;
            return Err(e);
        }
        Ok(())
    }
    fn untouched(&self, a: u32) -> bool {
        !self.failed.iter().any(|&f| f & !3 == a & !3)
    }
}

impl Bus for Faulty {
    fn read(&mut self, w: Width, addr: u32) -> BusResult<u32> {
        self.hit(addr)?;
        self.b.read(w, addr)
    }
    fn write(&mut self, w: Width, addr: u32, val: u32) -> BusResult<()> {
        self.hit(addr)?;
        self.b.write(w, addr, val)
    }
    fn rmw(&mut self, w: Width, addr: u32, clear: u32, set: u32) -> BusResult<()> {
        self.hit(addr)?;
        self.b.rmw(w, addr, clear, set)
    }
    fn poll(&mut self, w: Width, addr: u32, mask: u32, want: u32, ms: u16) -> BusResult<()> {
        self.hit(addr)?;
        self.b.poll(w, addr, mask, want, ms)
    }
    fn delay_us(&mut self, us: u32) -> BusResult<()> {
        self.hit(u32::MAX)?;
        self.b.delay_us(us)
    }
    fn cfg_read(&mut self, off: u16) -> BusResult<u32> {
        self.b.cfg_read(off)
    }
    fn cfg_write(&mut self, off: u16, val: u32) -> BusResult<()> {
        self.b.cfg_write(off, val)
    }
    fn dma_alloc(&mut self, size: usize, align: usize) -> BusResult<u32> {
        self.b.dma_alloc(size, align)
    }
    fn dma_free(&mut self, addr: u32, size: usize) -> BusResult<()> {
        self.b.dma_free(addr, size)
    }
    fn mem_write(&mut self, w: MemWidth, addr: u32, val: u64) -> BusResult<()> {
        self.b.mem_write(w, addr, val)
    }
    fn mem_read32(&mut self, addr: u32) -> BusResult<u32> {
        self.b.mem_read32(addr)
    }
    fn mem_write_bytes(&mut self, addr: u32, data: &[u8]) -> BusResult<()> {
        self.b.mem_write_bytes(addr, data)
    }
    fn mem_read_bytes(&mut self, addr: u32, out: &mut [u8]) -> BusResult<()> {
        self.b.mem_read_bytes(addr, out)
    }
    fn now_ms(&mut self) -> u64 {
        self.b.now_ms()
    }
}

fn injected(addr: u32) -> BusError {
    BusError::Refused {
        kind: "injected",
        addr,
        len: 4,
    }
}

// a chip, state and phy after a good switch to channel 1; `rule` applies from here on
fn after_ch1(rule: Rule) -> (Faulty, ChanState, Phy) {
    let mut f = Faulty::new(chip(), Box::new(|_, _| None));
    let mut st = ready();
    let mut p = phy();
    let mut h = Calls::default();
    set_channel(&mut f, &mut p, &mut st, req(Band::B2g, 1), &mut h, false).unwrap();
    st.take_events();
    f.n = 0;
    f.rule = rule;
    (f, st, p)
}

#[test]
fn every_failing_op_still_leaves_the_help_state() {
    let (mut f, mut st, mut p) = after_ch1(Box::new(|_, _| None));
    let mut h = Calls::default();
    set_channel(&mut f, &mut p, &mut st, req(Band::B5g, 36), &mut h, false).unwrap();
    let total = f.n;
    assert!(total > 150, "{total} ops");
    for at in 1..=total {
        let (mut f, mut st, mut p) =
            after_ch1(Box::new(move |n, a| (n == at).then(|| injected(a))));
        let mut h = Calls::default();
        let r = set_channel(&mut f, &mut p, &mut st, req(Band::B5g, 36), &mut h, false);
        let addr = f.failed[0];
        assert_eq!(r, Err(Rtw89Error::Bus(injected(addr))), "op {at}");
        assert!(!st.entity_active, "op {at}");
        let ev = st.take_events();
        assert!(
            matches!(ev.last(), Some(ChanEvent::Aborted { .. })),
            "op {at}: {ev:?}"
        );
        assert!(
            !ev.iter().any(|e| matches!(e, ChanEvent::UndoFailed { .. })),
            "op {at}: one failure only"
        );
        if at <= 2 {
            // the stop itself failed: nothing was stopped, nothing to put back
            assert_eq!(h.0, ["stop"], "op {at}");
            continue;
        }
        assert!(
            h.0.iter().any(|c| c == "resume 0xff"),
            "op {at}: TX resumed ({:?})",
            h.0
        );
        assert!(
            !h.0.iter().any(|c| c.starts_with("btc")),
            "op {at}: no band change"
        );
        if f.untouched(R_AX_CTN_TXEN) {
            assert_eq!(
                f.b.regs.get(R_AX_CTN_TXEN, Width::W16),
                0xff,
                "op {at}: TX back"
            );
        }
        // the help's leave ran: ADC FIFO out of reset, TSSI tracking on, BB reset released
        if f.untouched(BB + R_ADC_FIFO) {
            assert_eq!(f.b.bb(R_ADC_FIFO) & B_ADC_FIFO_RST, 0, "op {at}: ADC");
        }
        if f.untouched(BB + R_P0_TXPW_RSTB) {
            assert_eq!(
                f.b.bb(R_P0_TXPW_RSTB) & B_P0_TXPW_RSTB_MANON,
                0,
                "op {at}: TSSI"
            );
        }
        if f.untouched(BB + R_RSTB_ASYNC) {
            assert_ne!(
                f.b.bb(R_RSTB_ASYNC) & B_RSTB_ASYNC_ALL,
                0,
                "op {at}: BB reset"
            );
        }
        if f.untouched(R_AX_PPDU_STAT) && f.untouched(R_AX_CMAC_FUNC_EN) {
            assert_ne!(
                f.b.regs.get(R_AX_PPDU_STAT, Width::W32) & B_AX_PPDU_STAT_RPT_EN,
                0,
                "op {at}: PPDU status"
            );
        }
    }
}

#[test]
fn later_failures_are_events_and_a_gone_chip_gets_no_more_access() {
    // the RF step fails, then the leave's PPDU status access too (the enter's is op 4):
    // UndoFailed, the rest still runs
    let mut f = Faulty::new(
        chip(),
        Box::new(|n, a| (n > 10 && a == R_AX_PPDU_STAT).then(|| injected(a))),
    );
    let mut st = ready();
    let mut p = phy();
    let mut h = OneFails(Calls::default(), "rf");
    let r = set_channel(&mut f, &mut p, &mut st, req(Band::B2g, 1), &mut h, false);
    assert!(matches!(
        r,
        Err(Rtw89Error::Bus(BusError::PollTimeout { .. }))
    ));
    let ev = st.take_events();
    assert!(
        ev.iter().any(|e| matches!(
            e,
            ChanEvent::UndoFailed {
                step: "ppdu status on",
                ..
            }
        )),
        "{ev:?}"
    );
    assert_eq!(h.0.0.last().map(String::as_str), Some("resume 0xff"));
    assert_eq!(f.b.bb(R_ADC_FIFO) & B_ADC_FIFO_RST, 0);
    for e in &ev {
        assert!(!e.to_string().is_empty());
    }
    // the chip goes away during the switch: no access after the one that failed
    for at in [3u64, 12, 40, 80, 150] {
        let (mut f, mut st, mut p) =
            after_ch1(Box::new(move |n, _| (n == at).then_some(BusError::Gone)));
        let mut h = Calls::default();
        let r = set_channel(&mut f, &mut p, &mut st, req(Band::B5g, 36), &mut h, false);
        assert_eq!(r, Err(Rtw89Error::Bus(BusError::Gone)), "op {at}");
        assert_eq!(f.after_gone, 0, "op {at}: no access after Gone");
    }
}

#[test]
fn events_of_the_cmac_and_the_txpwr_check() {
    let mut st = ready();
    let mut b = chip();
    let dead = alloc::rc::Rc::new(core::cell::Cell::new(3u32));
    let d2 = dead.clone();
    b.answer_fn(Key::Mac(R_AX_WMAC_RFMOD), move |_| {
        if d2.get() > 0 {
            d2.set(d2.get() - 1);
            RTW89_R32_DEAD
        } else {
            0x12
        }
    });
    set_channel_mac(&mut b, &chan_create(Band::B2g, 1), &mut st).unwrap();
    assert_eq!(
        st.take_events(),
        [ChanEvent::CmacRetry {
            addr: R_AX_WMAC_RFMOD,
            tries: 3,
            value: 0x12
        }]
    );
    // the CMAC off: the PPDU status is left alone
    let mut b = chip();
    b.answer(Key::Mac(R_AX_CMAC_FUNC_EN), 0);
    let mut h = Calls::default();
    let tx = help_enter(&mut b, &mut st, Band::B2g, &mut h).unwrap();
    help_leave(&mut b, &mut st, Band::B2g, &mut h, tx).unwrap();
    assert_eq!(
        st.take_events(),
        [
            ChanEvent::PpduStatusSkipped {
                enable: false,
                value: 0
            },
            ChanEvent::PpduStatusSkipped {
                enable: true,
                value: 0
            }
        ]
    );
    // a TX power register outside the CMAC 0 window is refused and noted
    let mut b = chip();
    let mut ev = crate::events::EventLog::default();
    mac_txpwr_w32(&mut b, 0x1000, 1, &mut ev).unwrap();
    mac_txpwr_w32(&mut b, CMAC1_START_ADDR_AX, 1, &mut ev).unwrap();
    assert!(b.trace.is_empty());
    assert_eq!(
        ev.take(),
        [
            ChanEvent::TxpwrCrRefused { addr: 0x1000 },
            ChanEvent::TxpwrCrRefused {
                addr: CMAC1_START_ADDR_AX
            }
        ]
    );
    assert!(
        ChanEvent::TxpwrCrRefused { addr: 1 }
            .to_string()
            .contains("txpwr cr")
    );
}

/// rfk1's neighbours noting their calls
#[derive(Default)]
struct Rfk(Vec<&'static str>);

impl<B: Bus + ?Sized> RfkHooks<B> for Rfk {
    fn btc_wl_rfk(&mut self, _: &mut B, _: u32, _: u32, _: u32) -> Rtw89Result<()> {
        Ok(())
    }
    fn btc_conn_rfk(&mut self, _: &mut B, _: bool) -> Rtw89Result<()> {
        self.0.push("conn");
        Ok(())
    }
    fn btc_preserve_bt_time(&mut self, _: &mut B, _: u32) -> Rtw89Result<()> {
        Ok(())
    }
    fn stop_sch_tx(&mut self, bus: &mut B) -> Rtw89Result<u16> {
        self.0.push("stop");
        Ok(bus.r16(R_AX_CTN_TXEN)?)
    }
    fn resume_sch_tx(&mut self, _: &mut B, _: u16) -> Rtw89Result<()> {
        self.0.push("resume");
        Ok(())
    }
    fn h2c(&mut self, _: &mut B, _: &crate::fw::H2c, _: &[u8]) -> Rtw89Result<()> {
        self.0.push("h2c");
        Ok(())
    }
    fn dpk_init(&mut self, _: &mut B, _: &mut Phy) -> Rtw89Result<()> {
        Ok(())
    }
    fn dpk(&mut self, _: &mut B, _: &mut Phy, _: Option<&Chan>, _: u32) -> Rtw89Result<()> {
        self.0.push("dpk");
        Ok(())
    }
    fn dpk_track(&mut self, _: &mut B, _: &mut Phy) -> Rtw89Result<()> {
        Ok(())
    }
    fn tssi(
        &mut self,
        _: &mut B,
        _: &mut Phy,
        _: Option<&Chan>,
        _: u32,
        _: bool,
    ) -> Rtw89Result<()> {
        self.0.push("tssi");
        Ok(())
    }
    fn tssi_scan(&mut self, _: &mut B, _: &mut Phy, _: Option<&Chan>, _: u32) -> Rtw89Result<()> {
        self.0.push("tssi_scan");
        Ok(())
    }
    fn wifi_scan_notify(&mut self, _: &mut B, _: &mut Phy, _: bool, _: u32) -> Rtw89Result<()> {
        Ok(())
    }
}

#[test]
fn chan_link_reaches_rfk1() {
    let mut b = RfFake::fresh();
    b.answer(Key::Mac(R_AX_CMAC_FUNC_EN), B_AX_CMAC_EN);
    let mut st = ready();
    let mut p = phy();
    let mut rfk = RfkState::new();
    let mut hooks = Rfk::default();
    let mut bands = Vec::new();
    let mut btc = |_: &mut RfFake, band: Band, _: &mut dyn RfkHooks<RfFake>| {
        bands.push(band);
        Ok(())
    };
    let mut link = ChanLink {
        rfk: &mut rfk,
        hooks: &mut hooks,
        btc_switch_band: &mut btc,
    };
    set_channel(&mut b, &mut p, &mut st, req(Band::B2g, 1), &mut link, true).unwrap();
    assert_eq!(bands, [Band::B2g]);
    let seen = hooks.0.clone();
    assert_eq!(
        seen[..3],
        ["stop", "resume", "tssi_scan"],
        "the help's stop/resume, then the band change's TSSI scan"
    );
    for want in ["conn", "tssi", "dpk", "h2c"] {
        assert!(seen[3..].contains(&want), "rfk_channel's {want}: {seen:?}");
    }
    // rfk1's RF channel set wrote RF 0x18 of both paths with channel 1
    assert_eq!(b.peek(0, RR_CFGCH) & 0xff, 1);
    assert_eq!(b.peek(1, RR_CFGCH) & 0xff, 1);
}

#[test]
fn the_event_log_is_bounded() {
    let mut b = chip();
    let mut st = ready();
    let mut p = phy();
    st.set_event_cap(2);
    for ch in [1, 6, 11, 1, 6] {
        let mut h = Calls::default();
        set_channel(&mut b, &mut p, &mut st, req(Band::B2g, ch), &mut h, false).unwrap();
    }
    assert_eq!(st.events().len(), 2);
    assert_eq!(st.events_dropped(), 3);
}

#[test]
fn cfg_txrx_path_takes_rx_nss_from_the_phycap() {
    // both paths: rx_nss as mac_setup_phycap left it (0 = the chip's 2); one path: 1
    for (ant, nss, two) in [(0, 0, 1), (0, 2, 1), (0, 1, 0), (RF_A, 2, 0), (RF_B, 0, 0)] {
        let mut b = RfFake::new(); // no fixed answer for R_CHBW_MOD_V1: its writes stay
        let mut st = ready();
        st.dm_init_done = false;
        let mut p = phy();
        p.hal.antenna_rx = ant;
        p.hal.rx_nss = nss;
        cfg_txrx_path(&mut b, &p, &mut st).unwrap();
        assert!(st.dm_init_done);
        let f = |reg, mask| crate::bus::field(b.bb(reg), mask);
        assert_eq!(
            f(R_RXHT_MCS_LIMIT, B_RXHT_MCS_LIMIT),
            two,
            "ant {ant} nss {nss}"
        );
        assert_eq!(f(R_RXHE, B_RXHE_MAX_NSS), two, "ant {ant} nss {nss}");
        let sel = match ant {
            RF_A => 1,
            RF_B => 2,
            _ => 3,
        };
        assert_eq!(f(R_CHBW_MOD_V1, B_ANT_RX_SEG0), sel, "ant {ant}");
    }
}

/// RfFake whose listed CMAC registers read RTW89_R32_DEAD once (clock gated), also inside an
/// RMW, as the chip would answer the read half of it
struct DeadOnce {
    b: RfFake,
    dead: Vec<u32>,
}

impl DeadOnce {
    fn take(&mut self, addr: u32) -> bool {
        let hit = self.dead.iter().position(|&a| a == addr);
        hit.map(|i| self.dead.remove(i)).is_some()
    }
}

impl Bus for DeadOnce {
    fn read(&mut self, w: Width, addr: u32) -> BusResult<u32> {
        if self.take(addr) {
            return Ok(RTW89_R32_DEAD & w.mask());
        }
        self.b.read(w, addr)
    }
    fn write(&mut self, w: Width, addr: u32, val: u32) -> BusResult<()> {
        self.b.write(w, addr, val)
    }
    fn rmw(&mut self, w: Width, addr: u32, clear: u32, set: u32) -> BusResult<()> {
        if self.take(addr) {
            return self
                .b
                .write(w, addr, ((RTW89_R32_DEAD & !clear) | set) & w.mask());
        }
        self.b.rmw(w, addr, clear, set)
    }
    fn poll(&mut self, w: Width, addr: u32, mask: u32, want: u32, ms: u16) -> BusResult<()> {
        self.b.poll(w, addr, mask, want, ms)
    }
    fn delay_us(&mut self, us: u32) -> BusResult<()> {
        self.b.delay_us(us)
    }
    fn cfg_read(&mut self, off: u16) -> BusResult<u32> {
        self.b.cfg_read(off)
    }
    fn cfg_write(&mut self, off: u16, val: u32) -> BusResult<()> {
        self.b.cfg_write(off, val)
    }
    fn dma_alloc(&mut self, size: usize, align: usize) -> BusResult<u32> {
        self.b.dma_alloc(size, align)
    }
    fn dma_free(&mut self, addr: u32, size: usize) -> BusResult<()> {
        self.b.dma_free(addr, size)
    }
    fn mem_write(&mut self, w: MemWidth, addr: u32, val: u64) -> BusResult<()> {
        self.b.mem_write(w, addr, val)
    }
    fn mem_read32(&mut self, addr: u32) -> BusResult<u32> {
        self.b.mem_read32(addr)
    }
    fn mem_write_bytes(&mut self, addr: u32, data: &[u8]) -> BusResult<()> {
        self.b.mem_write_bytes(addr, data)
    }
    fn mem_read_bytes(&mut self, addr: u32, out: &mut [u8]) -> BusResult<()> {
        self.b.mem_read_bytes(addr, out)
    }
    fn now_ms(&mut self) -> u64 {
        self.b.now_ms()
    }
}

// Linux reads every CMAC register through rtw89_pci_ops_read32_cmac, also inside
// write32_mask/clr32/set32. chan's RMWs of the CMAC (PPDU status, TX power registers) stay
// plain Bus RMWs as in the JS and the golden trace; the kernel driver gets the retry from
// deadread::CmacRead around its bus. With it, a gated clock on those reads leaves the same
// registers as a good run; without it, garbage bits land (the check has teeth).
#[test]
fn cmac_rmws_get_the_retry_from_cmac_read() {
    use crate::deadread::CmacRead;
    let regs = [R_AX_PPDU_STAT, R_AX_PWR_RATE_CTRL, R_AX_PWR_RATE_OFST_CTRL];
    let start = |b: &mut RfFake| {
        for (i, &a) in regs.iter().enumerate() {
            b.poke(a, Width::W32, 0x1230_0000 + i as u32);
        }
    };
    let run = |bus: &mut dyn Bus| {
        let mut st = ready();
        let mut p = phy();
        set_channel(
            bus,
            &mut p,
            &mut st,
            req(Band::B2g, 1),
            &mut Calls::default(),
            false,
        )
    };
    let mut good = chip();
    start(&mut good);
    run(&mut good).unwrap();
    let want: Vec<u32> = regs.iter().map(|&a| good.regs.get(a, Width::W32)).collect();

    let mut b = chip();
    start(&mut b);
    let mut bus = CmacRead::new(DeadOnce {
        b,
        dead: regs.to_vec(),
    });
    run(&mut bus).unwrap();
    assert!(bus.inner.dead.is_empty(), "every listed register was read");
    let got: Vec<u32> = regs
        .iter()
        .map(|&a| bus.inner.b.regs.get(a, Width::W32))
        .collect();
    assert_eq!(got, want);

    let mut b = chip();
    start(&mut b);
    let mut bare = DeadOnce {
        b,
        dead: regs.to_vec(),
    };
    run(&mut bare).unwrap();
    let got: Vec<u32> = regs
        .iter()
        .map(|&a| bare.b.regs.get(a, Width::W32))
        .collect();
    assert_ne!(got, want);
}
