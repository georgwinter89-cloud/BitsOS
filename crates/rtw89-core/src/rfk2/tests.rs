//! What the JS oracle (tests/rfk2_js.rs) cannot see: the events with their values, refusals
//! before any access, the TXPWR_TRK element checks, the soft poll passing other bus errors up,
//! and rfk2 inside rfk1 through `Rfk2Link`.

use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;

use super::*;
use crate::bus::{Width, field};
use crate::fw::H2c;
use crate::phy::efuse::Efuse;
use crate::phy::{Band, Chan};
use crate::rfk1::fake::{Key, RfFake};
use crate::rfk1::tests_hostile::{BUDGET, Hostile, Src};
use crate::rfk1::{self, RfkState};
use crate::trace::{OpKind, TraceEntry};

pub(super) const CH1: Chan = Chan {
    band: Band::B2g,
    ch: 1,
    bw: 20,
};

/// btc and macinit as test-rfk2.mjs's stand-ins: the notifications recorded, the scheduler on
/// its register
#[derive(Default)]
pub(super) struct Neigh {
    pub(super) btc: Vec<(u32, u32, u32)>,
    calls: Vec<&'static str>,
}

impl<B: Bus + ?Sized> Rfk2Hooks<B> for Neigh {
    fn btc_wl_rfk(&mut self, _: &mut B, phy_map: u32, typ: u32, state: u32) -> Rtw89Result<()> {
        self.btc.push((phy_map, typ, state));
        Ok(())
    }
    fn stop_sch_tx(&mut self, bus: &mut B) -> Rtw89Result<u16> {
        let tx = bus.r16(R_AX_CTN_TXEN)?;
        bus.mask(Width::W16, R_AX_CTN_TXEN, B_AX_CTN_TXEN_ALL_MASK, 0)?;
        Ok(tx)
    }
    fn resume_sch_tx(&mut self, bus: &mut B, tx_en: u16) -> Rtw89Result<()> {
        bus.mask(
            Width::W16,
            R_AX_CTN_TXEN,
            B_AX_CTN_TXEN_ALL_MASK,
            u32::from(tx_en),
        )?;
        Ok(())
    }
}

/// rfk1's neighbours for `Rfk2Link`: btc/mac/h2c recorded, rfk2's own entries must never run
impl<B: Bus + ?Sized> RfkHooks<B> for Neigh {
    fn btc_wl_rfk(&mut self, bus: &mut B, phy_map: u32, typ: u32, state: u32) -> Rtw89Result<()> {
        Rfk2Hooks::btc_wl_rfk(self, bus, phy_map, typ, state)
    }
    fn btc_conn_rfk(&mut self, _: &mut B, _: bool) -> Rtw89Result<()> {
        self.calls.push("btc_conn_rfk");
        Ok(())
    }
    fn btc_preserve_bt_time(&mut self, _: &mut B, _: u32) -> Rtw89Result<()> {
        self.calls.push("btc_preserve_bt_time");
        Ok(())
    }
    fn stop_sch_tx(&mut self, bus: &mut B) -> Rtw89Result<u16> {
        Rfk2Hooks::stop_sch_tx(self, bus)
    }
    fn resume_sch_tx(&mut self, bus: &mut B, tx_en: u16) -> Rtw89Result<()> {
        Rfk2Hooks::resume_sch_tx(self, bus, tx_en)
    }
    fn h2c(&mut self, _: &mut B, _: &H2c, _: &[u8]) -> Rtw89Result<()> {
        self.calls.push("h2c");
        Ok(())
    }
    fn dpk_init(&mut self, _: &mut B, _: &mut Phy) -> Rtw89Result<()> {
        unreachable!("Rfk2Link runs rfk2 itself")
    }
    fn dpk(&mut self, _: &mut B, _: &mut Phy, _: Option<&Chan>, _: u32) -> Rtw89Result<()> {
        unreachable!("Rfk2Link runs rfk2 itself")
    }
    fn dpk_track(&mut self, _: &mut B, _: &mut Phy) -> Rtw89Result<()> {
        unreachable!("Rfk2Link runs rfk2 itself")
    }
    fn tssi(
        &mut self,
        _: &mut B,
        _: &mut Phy,
        _: Option<&Chan>,
        _: u32,
        _: bool,
    ) -> Rtw89Result<()> {
        unreachable!("Rfk2Link runs rfk2 itself")
    }
    fn tssi_scan(&mut self, _: &mut B, _: &mut Phy, _: Option<&Chan>, _: u32) -> Rtw89Result<()> {
        unreachable!("Rfk2Link runs rfk2 itself")
    }
    fn wifi_scan_notify(&mut self, _: &mut B, _: &mut Phy, _: bool, _: u32) -> Rtw89Result<()> {
        unreachable!("Rfk2Link runs rfk2 itself")
    }
}

/// A phy with an efuse as the measured chip's (thermal 0x1a/0x18, no gain offsets)
pub(super) fn phy() -> Phy {
    let mut p = Phy::new(1);
    let mut e = Efuse::default();
    e.tssi.thermal = [0x1a, 0x18];
    p.efuse = Some(e);
    p
}

/// rfk2-tchip.mjs makeChip's good answers on RfFake (DPK sync good, TSSI reports ready)
pub(super) fn chip() -> RfFake {
    let mut b = RfFake::new();
    b.rf.insert((0, RR_TM), 0x1c << 1);
    b.rf.insert((1, RR_TM), 0x1d << 1);
    b.answer_fn(Key::Bb(R_RPT_COM), |r| {
        match field(r.bb(R_KIP_RPT1), B_KIP_RPT1_SEL) {
            0 => (0x600 << 16) | (200 << 8) | 5,
            9 => (10 << 16) | 0xff0,
            6 if field(r.bb(R_DPK_CFG2), B_DPK_CFG2_ST) != 0 => 0x30,
            _ => 0,
        }
    });
    b.answer(Key::Bb(R_DPD_BF), (16 << 12) | 28);
    for a in [0x1c18u32, 0x3c18] {
        b.answer_fn(Key::Bb(a), |r| {
            (1 << 16) | 0x100u32.wrapping_sub(field(r.bb(R_TXPWR), B_TXPWR_MSK))
        });
    }
    b.answer(Key::Bb(R_TXAGC_BB), 0x05fe_0021);
    b.answer(Key::Bb(R_TXAGC_BB + 0x2000), 0x05fe_0021);
    b.poke(R_AX_CTN_TXEN, Width::W16, 0xffff);
    b
}

#[test]
fn a_good_run_leaves_tssi_on_and_dpk_done() {
    let (mut b, mut p, mut st, mut n) = (chip(), phy(), Rfk2State::new(), Neigh::default());
    dpk_init(&mut b, &mut p, &mut st).unwrap();
    assert_eq!(
        st.dpk.dpk_gs[0], 0x7f,
        "backoff 16 + scale 28 moved to the BB"
    );
    tssi(&mut b, &mut p, &mut st, Some(&CH1), &mut n, 0, true).unwrap();
    assert_eq!(p.tssi_mode, [true, true]);
    assert!(st.tssi.alignment_done[0][0] && st.tssi.alignment_done[1][0]);
    assert!(
        st.tssi.check_backup_aligmk[1][0],
        "channel 1 kept for later"
    );
    dpk(&mut b, &mut p, &mut st, Some(&CH1), &mut n, 0).unwrap();
    assert!(st.dpk.bp[0][0].path_ok && st.dpk.bp[1][0].path_ok);
    assert_eq!(st.dpk.bp[0][0].ther_dpk, 0x1c);
    assert_eq!(
        b.regs.get(R_AX_CTN_TXEN, Width::W16),
        0xffff,
        "scheduler back on"
    );
    assert_eq!(
        n.btc,
        [
            (0x13, BTC_WRFKT_IQK, BTC_WRFK_ONESHOT_START),
            (0x13, BTC_WRFKT_IQK, BTC_WRFK_ONESHOT_STOP),
            (0x10, BTC_WRFKT_DPK, BTC_WRFK_START),
            (0x10, BTC_WRFKT_DPK, BTC_WRFK_STOP),
        ],
        "coex: TSSI as IQK one-shot (paths AB), DPK start/stop (paths 0)"
    );
    assert!(st.events().is_empty(), "{:?}", st.events());
    assert_eq!(st.poll_timeouts, 0);
}

#[test]
fn timeouts_and_failures_are_events_with_their_values() {
    // the KIP one-shot never answers: both waits noted, with register and value
    let mut b = chip();
    b.fail_polls.insert(BB + 0xbff8);
    b.poke(BB + 0xbff8, Width::W32, 0x12);
    let mut st = Rfk2State::new();
    dpk::dpk_one_shot(&mut b, &mut st, 0, 1, dpk::SYNC).unwrap();
    assert_eq!(
        st.take_events(),
        [Rfk2Event::OneShotTimeout {
            path: 1,
            addr: 0xbff8,
            last: 0x12
        }]
    );
    assert_eq!(st.poll_timeouts, 1);
    let polls = b
        .trace
        .iter()
        .filter(|e| matches!(e, TraceEntry::Op(o) if o.kind == OpKind::Poll))
        .count();
    assert_eq!(polls, 2, "the one-shot goes on after a timeout, as Linux");

    // a bad sync: each path's DPK failed, DPD off
    let mut b = chip();
    b.answer_fn(Key::Bb(R_RPT_COM), |r| {
        if field(r.bb(R_KIP_RPT1), B_KIP_RPT1_SEL) == 0 {
            (0x600 << 16) | (150 << 8) | 5
        } else {
            0
        }
    });
    let (mut p, mut st, mut n) = (phy(), Rfk2State::new(), Neigh::default());
    dpk(&mut b, &mut p, &mut st, Some(&CH1), &mut n, 0).unwrap();
    assert_eq!(
        st.take_events(),
        [0, 1].map(|path| Rfk2Event::DpkFailed {
            path,
            kidx: 0,
            corr_val: 150,
            dc_i: 0,
            dc_q: 0
        })
    );
    assert!(!st.dpk.bp[0][0].path_ok && !st.dpk.bp[1][0].path_ok);

    // no TSSI report: the path keeps its default alignment
    let mut b = chip();
    b.answer(Key::Bb(0x1c18), 0);
    b.answer(Key::Bb(0x3c18), 0);
    let (mut p, mut st) = (phy(), Rfk2State::new());
    tssi(&mut b, &mut p, &mut st, Some(&CH1), &mut n, 0, true).unwrap();
    assert_eq!(
        st.take_events(),
        [0, 1].map(|path| Rfk2Event::TssiNoReport { path, power: 0x30 })
    );
    assert_eq!(st.tssi.alignment_done, [[false; ALIMK_MAX]; 2]);

    // RF mode stuck in TX on path A
    let mut b = chip();
    b.rf.insert((0, RR_MOD), prep(RR_MOD_MASK, 2));
    let (mut p, mut st) = (phy(), Rfk2State::new());
    dpk(&mut b, &mut p, &mut st, Some(&CH1), &mut n, 0).unwrap();
    assert_eq!(
        st.events()[0],
        Rfk2Event::RxModeTimeout { path: 0, mode: 2 }
    );

    // an external PA: DPK skipped, DPD off on both paths
    let mut b = chip();
    let (mut p, mut st) = (phy(), Rfk2State::new());
    st.fem.epa_2g = true;
    dpk(&mut b, &mut p, &mut st, Some(&CH1), &mut n, 0).unwrap();
    assert_eq!(
        st.take_events(),
        [Rfk2Event::DpkSkipped {
            band: RTW89_BAND_2G
        }]
    );
    let lines = b.lines();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("rmw32 0x000181BC 0xFF000000")),
        "DPD path A switched by _dpk_onoff: {lines:?}"
    );
}

#[test]
fn events_display_and_cap() {
    let all = [
        Rfk2Event::OneShotTimeout {
            path: 1,
            addr: 0xbff8,
            last: 0x12,
        },
        Rfk2Event::RxModeTimeout { path: 0, mode: 2 },
        Rfk2Event::DpkFailed {
            path: 1,
            kidx: 0,
            corr_val: 150,
            dc_i: 3,
            dc_q: 4,
        },
        Rfk2Event::DpkSkipped { band: 0 },
        Rfk2Event::TssiNoReport { path: 0, power: 48 },
        Rfk2Event::TxpwrTrkDefault,
    ];
    let text: Vec<_> = all.iter().map(ToString::to_string).collect();
    assert_eq!(text[0], "[DPK] S1 one-shot timeout (0xbff8 = 0x12)");
    assert_eq!(text[2], "[DPK] S1[0] DPK fail (corr 150, dc 3/4)");
    assert_eq!(text[4], "[TSSI PA K] S0 TSSI report not ready (power 48)");
    assert!(text.iter().all(|t| !t.is_empty()));
    let mut st = Rfk2State::new();
    st.set_event_cap(2);
    for e in all {
        st.push(e);
    }
    assert_eq!(st.events().len(), 2);
    assert_eq!(st.events_dropped(), 4);
    assert_eq!(st.take_events()[1], Rfk2Event::TxpwrTrkDefault);
}

#[test]
fn refused_before_any_access() {
    let c6 = Chan {
        band: Band::B6g,
        ch: 1,
        bw: 20,
    };
    let c160 = Chan {
        band: Band::B5g,
        ch: 50,
        bw: 160,
    };
    let mut n = Neigh::default();
    let mut st = Rfk2State::new();
    let none = |b: &RfFake| b.trace.is_empty();
    // no efuse: TSSI refuses
    let mut b = chip();
    let mut bare = Phy::new(1);
    assert!(matches!(
        tssi(&mut b, &mut bare, &mut st, Some(&CH1), &mut n, 0, true),
        Err(Rtw89Error::Invalid { .. })
    ));
    assert!(matches!(
        tssi_scan(&mut b, &mut bare, &mut st, Some(&CH1), 0),
        Err(Rtw89Error::Invalid { .. })
    ));
    assert!(none(&b) && n.btc.is_empty());
    // channels rfk2 has no code for
    let mut p = phy();
    for c in [c6, c160] {
        assert!(dpk(&mut b, &mut p, &mut st, Some(&c), &mut n, 0).is_err());
        assert!(tssi(&mut b, &mut p, &mut st, Some(&c), &mut n, 0, true).is_err());
        assert!(tssi_scan(&mut b, &mut p, &mut st, Some(&c), 0).is_err());
        assert!(wifi_scan_notify(&mut b, &mut p, &mut st, Some(&c), &mut n, true, 0).is_err());
    }
    // a PHY past the two
    assert_eq!(
        dpk(&mut b, &mut p, &mut st, Some(&CH1), &mut n, 2),
        Err(Rtw89Error::Invalid {
            what: "rfk2: PHY index past the chip's two",
            value: 2
        })
    );
    // state with a slot past the two
    st.dpk.cur_idx = [0, 5];
    assert!(matches!(
        dpk_track(&mut b, &mut p, &mut st),
        Err(Rtw89Error::Invalid {
            what: "rfk2: dpk cur_idx past the slots",
            value: 5
        })
    ));
    assert!(none(&b) && n.btc.is_empty(), "{:?}", b.lines());
    // paths past the two in the steps
    let rc = crate::rfk1::util::chan_get(Some(&CH1)).unwrap();
    for path in [2, 3, u32::MAX] {
        assert!(dpk::dpk_one_shot(&mut b, &mut st, 0, path, dpk::SYNC).is_err());
        assert!(dpk::dpk_tssi_pause(&mut b, path, true).is_err());
        assert!(dpk::dpk_kip_restore(&mut b, path).is_err());
        assert!(dpk::dpk_table_select(&mut b, path, 0, 1).is_err());
        assert!(dpk::dpk_kip_pwr_clk_on(&mut b, path).is_err());
        assert!(dpk::dpk_rf_setting(&mut b, &mut st, 1, path, 0).is_err());
        assert!(dpk::dpk_fill_result(&mut b, &mut st, 0, path, 0, 1, 0x32).is_err());
        assert!(tssi::tssi_set_sys(&mut b, path, &rc).is_err());
        assert!(tssi::tssi_set_tmeter_tbl(&mut b, &p, &st, path, &rc).is_err());
        assert!(tssi::tssi_alignment_default(&mut b, path, true, &rc).is_err());
        assert!(tssi::tssi_alimentk_done(&mut b, &st, path, &rc).is_err());
        assert!(alimk::tssi_alimentk(&mut b, &p, &mut st, path, &rc).is_err());
    }
    assert!(dpk::dpk_fill_result(&mut b, &mut st, 0, 0, 2, 1, 0x32).is_err());
    assert!(dpk::dpk_fill_result(&mut b, &mut st, 2, 0, 0, 1, 0x32).is_err());
    assert!(none(&b), "{:?}", b.lines());
}

#[test]
fn the_soft_poll_passes_other_bus_errors_up() {
    let mut b = Hostile::new(Src::Const(0), 1);
    b.ops = BUDGET; // the next op finds the chip gone
    let mut st = Rfk2State::new();
    assert_eq!(
        poll_soft(&mut b, &mut st, 0, 0xbff8, MASKBYTE0, 0x55, 20000),
        Err(Rtw89Error::Bus(BusError::Gone))
    );
    assert_eq!(st.poll_timeouts, 0);
    assert!(st.events().is_empty());
}

/// struct rtw89_fw_element_hdr's union u for TXPWR_TRK: bitmap, reserved, rows of 30 s8
fn elm(bitmap: u32, rows: usize) -> Vec<u8> {
    let mut v = vec![0u8; 8 + rows * 30];
    v[..4].copy_from_slice(&bitmap.to_le_bytes());
    for (i, x) in v[8..].iter_mut().enumerate() {
        *x = ((i % 30) as u8) ^ (((i / 30) as u8) << 4);
    }
    v
}

#[test]
fn txpwr_trk_elements_as_linux() {
    // 2 and 5 GHz types: 4 x 3 + 8 x 1 rows
    let p = elm(trk::NEEDED, 20);
    let t = trk::build_from_elm(&p, 600, 0, 0).unwrap().unwrap();
    assert_eq!(t.delta[RTW89_FW_TXPWR_TRK_TYPE_5GB_N as usize].len(), 3);
    assert_eq!(t.delta[RTW89_FW_TXPWR_TRK_TYPE_2GA_P as usize].len(), 1);
    assert!(t.delta[0].is_empty(), "no 6 GHz");
    assert_eq!(
        t.delta[4][1][2],
        2 ^ (1 << 4),
        "5GB_N row 1 is the element's row 1"
    );
    assert_eq!(
        t.delta[15][0][29],
        (29 ^ (19 << 4)) as u8 as i8,
        "2G_CCK_A_P: row 19"
    );
    // the aid: another die's element is ignored, the own taken
    assert_eq!(trk::build_from_elm(&p, 600, 7, 0), Ok(None));
    assert!(trk::build_from_elm(&p, 600, 7, 7).unwrap().is_some());
    // where Linux fails the firmware
    let short = trk::NEEDED & !(1 << 10);
    assert_eq!(
        trk::build_from_elm(&elm(short, 20), 600, 0, 0),
        Err(Rtw89Error::FwFile {
            what: "needed txpwr trk bitmap missing",
            value: short
        })
    );
    assert!(matches!(
        trk::build_from_elm(&p, 599, 0, 0),
        Err(Rtw89Error::FwFile {
            what: "unexpected txpwr trk offset over size",
            ..
        })
    ));
    assert!(matches!(
        trk::build_from_elm(&p[..8 + 19 * 30 + 5], 600, 0, 0),
        Err(Rtw89Error::FwFile {
            what: "txpwr trk row past the file's end",
            ..
        })
    ));
    assert!(trk::build_from_elm(&p[..3], 600, 0, 0).is_err());
    // a 6 GHz bit as well: 4 more rows
    let p6 = elm(trk::NEEDED | 1, 24);
    let t6 = trk::build_from_elm(&p6, 720, 0, 0).unwrap().unwrap();
    assert_eq!(t6.delta[0].len(), 4);

    // loading: the last element wins; none = Linux's table (event); an error leaves Linux's
    let mut st = Rfk2State::new();
    st.load_txpwr_trk_from([(&p6[..], 720, 0), (&p[..], 600, 0)], 0)
        .unwrap();
    assert_eq!(st.txpwr_trk.as_ref(), Some(&t));
    assert_eq!(
        trk::row(&st, RTW89_FW_TXPWR_TRK_TYPE_2GA_N, 0).unwrap(),
        &t.delta[10][0]
    );
    assert!(trk::row(&st, 0, 0).is_err(), "no 6 GHz rows in this one");
    assert!(trk::row(&st, 4, 3).is_err() && trk::row(&st, 16, 0).is_err());
    st.load_txpwr_trk_from([], 0).unwrap();
    assert_eq!(st.take_events(), [Rfk2Event::TxpwrTrkDefault]);
    assert_eq!(
        trk::row(&st, 10, 0).unwrap(),
        &crate::tables::trk::TRK_CFG[10].unwrap()[0]
    );
    assert!(
        st.load_txpwr_trk_from([(&p[..], 1, 0)], 0).is_err() && st.txpwr_trk.is_none(),
        "a bad element leaves Linux's own table"
    );
}

#[test]
fn rfk2_link_runs_rfk2_inside_rfk1() {
    let mut b = chip();
    b.answer(Key::Bb(0xbff8), 0x55);
    let (mut p, mut st2, mut st1, mut n) =
        (phy(), Rfk2State::new(), RfkState::new(), Neigh::default());
    let mut link = Rfk2Link {
        st: &mut st2,
        hooks: &mut n,
        chan: Some(CH1),
    };
    rfk1::rfk_init(&mut b, &mut p, &mut st1, None, &mut link).unwrap();
    assert_eq!(link.st.dpk.dpk_gs[0], 0x7f, "rfk2's dpk_init ran");
    let first: Vec<_> = b.lines().into_iter().take(2).collect();
    assert_eq!(
        first, ["rd32 0x000144A0 0x00000000 0x00000000"; 2],
        "R_DPD_BF read twice first, as the golden trace"
    );
    rfk1::rfk_channel(&mut b, &mut p, &mut st1, Some(&CH1), &mut link, 0).unwrap();
    assert_eq!(p.tssi_mode, [true, true], "rfk2's tssi ran");
    assert!(
        link.st.dpk.is_dpk_enable && link.st.dpk.bp[0][0].ch == 1,
        "rfk2's dpk ran"
    );
    let n0 = b.trace.len();
    rfk1::rfk_scan(&mut b, &mut p, true, &mut link, 0).unwrap();
    assert_eq!(b.trace.len(), n0, "scan start with TSSI on: nothing to do");
    rfk1::rfk_scan(&mut b, &mut p, false, &mut link, 0).unwrap();
    assert_eq!(
        b.trace.len() - n0,
        6 + 8,
        "scan end: six TSSI offset writes, the stored alignment on both paths"
    );
    p.avg_thermal = [0x1a, 0x1a];
    rfk1::rfk_track(&mut b, &mut p, &mut link).unwrap();
    rfk1::rfk_band_changed(&mut b, &mut p, Some(&CH1), &mut link).unwrap();
    assert!(link.st.events().is_empty(), "{:?}", link.st.events());
    assert!(n.calls.contains(&"btc_conn_rfk") && n.calls.contains(&"h2c"));
}
